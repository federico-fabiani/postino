//! A scripted OAuth 2.0 token endpoint over TLS, and the XOAUTH2 payload shape.
//!
//! The endpoint speaks just enough HTTP/1.1 for `postino-net::oauth`: one request, a status line,
//! `Content-Length`, `Connection: close`, and a body a test scripts. Every request body is recorded
//! so a test can assert the PKCE verifier or the refresh grant without reading client internals.

use std::sync::{Arc, Mutex};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

use crate::TlsFixture;

/// One scripted token-endpoint reply.
#[derive(Debug, Clone)]
pub struct TokenReply {
    /// HTTP status line code.
    pub status: u16,
    /// Raw response body.
    pub body: String,
}

impl TokenReply {
    /// A `200` with the given JSON body.
    #[must_use]
    pub fn json(body: impl Into<String>) -> Self {
        Self { status: 200, body: body.into() }
    }

    /// A refusal with a provider error code, as Google writes it.
    #[must_use]
    pub fn error(status: u16, code: &str) -> Self {
        Self { status, body: format!("{{\"error\":\"{code}\"}}") }
    }
}

/// The JSON a successful authorization returns.
#[must_use]
pub fn token_json(access: &str, refresh: Option<&str>, expires_in: u64) -> String {
    match refresh {
        Some(refresh) => {
            format!("{{\"access_token\":\"{access}\",\"refresh_token\":\"{refresh}\",\"expires_in\":{expires_in},\"token_type\":\"Bearer\"}}")
        }
        None => format!("{{\"access_token\":\"{access}\",\"expires_in\":{expires_in},\"token_type\":\"Bearer\"}}"),
    }
}

/// The base64 SASL XOAUTH2 payload a client must send for `user` and `token`.
#[must_use]
pub fn expected_xoauth2(user: &str, token: &str) -> String {
    STANDARD.encode(format!("user={user}\x01auth=Bearer {token}\x01\x01"))
}

/// A TLS listener that answers OAuth requests with scripted replies, one per connection.
pub struct FakeOAuth {
    port: u16,
    task: JoinHandle<()>,
    requests: Arc<Mutex<Vec<String>>>,
}

impl FakeOAuth {
    /// Start on loopback. The certificate is the fixture's `localhost` leaf, so endpoints are
    /// `https://localhost:<port>/...`.
    pub async fn start(fixture: &TlsFixture, replies: Vec<TokenReply>) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.expect("bind fake oauth");
        let port = listener.local_addr().expect("oauth address").port();
        let acceptor = fixture.acceptor();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&requests);
        let task = tokio::spawn(async move {
            for reply in replies {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let Ok(mut stream) = acceptor.accept(stream).await else {
                    continue;
                };
                let mut buffer = Vec::new();
                let mut chunk = [0u8; 2048];
                loop {
                    if let Some(index) = find_headers_end(&buffer) {
                        let head = String::from_utf8_lossy(buffer.get(..index).unwrap_or_default()).to_ascii_lowercase();
                        let length = head
                            .lines()
                            .find_map(|line| line.strip_prefix("content-length:"))
                            .and_then(|value| value.trim().parse::<usize>().ok())
                            .unwrap_or(0);
                        let body_start = index + 4;
                        while buffer.len() < body_start + length {
                            match stream.read(&mut chunk).await {
                                Ok(0) | Err(_) => break,
                                Ok(read) => buffer.extend_from_slice(&chunk[..read]),
                            }
                        }
                        let body = buffer.get(body_start..body_start + length).unwrap_or_default().to_vec();
                        recorded.lock().expect("oauth requests").push(String::from_utf8_lossy(&body).into_owned());
                        break;
                    }
                    match stream.read(&mut chunk).await {
                        Ok(0) | Err(_) => break,
                        Ok(read) => buffer.extend_from_slice(&chunk[..read]),
                    }
                }
                let response = format!(
                    "HTTP/1.1 {} Scripted\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    reply.status,
                    reply.body.len(),
                    reply.body
                );
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.flush().await;
                let _ = stream.shutdown().await;
            }
        });
        Self { port, task, requests }
    }

    /// `https://localhost:<port>/token`.
    #[must_use]
    pub fn token_endpoint(&self) -> String {
        format!("https://localhost:{}/token", self.port)
    }

    /// `https://localhost:<port>/revoke`.
    #[must_use]
    pub fn revoke_endpoint(&self) -> String {
        format!("https://localhost:{}/revoke", self.port)
    }

    /// Port the endpoint listens on.
    #[must_use]
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Form bodies received so far, in order.
    #[must_use]
    pub fn requests(&self) -> Vec<String> {
        self.requests.lock().expect("oauth requests").clone()
    }

    /// Wait for the scripted connections to finish.
    pub async fn finish(self) {
        let _ = self.task.await;
    }
}

fn find_headers_end(buffer: &[u8]) -> Option<usize> {
    buffer.windows(4).position(|window| window == b"\r\n\r\n")
}
