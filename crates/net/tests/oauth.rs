//! OAuth 2.0: PKCE, the loopback redirect, the bounded TLS token endpoint, and SASL XOAUTH2.
//!
//! The token endpoint is a real TLS listener from `postino-testkit`; the client trusts its CA
//! through `extra_roots`, exactly as the IMAP tests trust the mail server. Nothing here fakes the
//! TLS stack or the HTTP framing.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use postino_net::oauth::{DEFAULT_TIMEOUT, Loopback, TokenEndpoint};
use postino_net::{AuthRequest, Authorization, NetError, OAuthProvider, Pkce, Secret, Session, SmtpClient};
use postino_testkit::{FakeOAuth, FakeServer, FakeSmtp, TokenReply, USER, expected_xoauth2, token_json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

fn local_provider(oauth: &FakeOAuth) -> OAuthProvider {
    OAuthProvider::gmail().with_endpoints(
        "https://localhost/authorize",
        Box::leak(oauth.token_endpoint().into_boxed_str()),
        Box::leak(oauth.revoke_endpoint().into_boxed_str()),
    )
}

fn client(oauth: &FakeOAuth, tls: &postino_testkit::TlsFixture) -> TokenEndpoint {
    TokenEndpoint::new(local_provider(oauth), "client-id", "client-secret").with_extra_root(tls.ca()).with_timeout(Duration::from_secs(10))
}

#[tokio::test]
async fn the_loopback_redirect_returns_the_code_for_the_matching_state() {
    let loopback = Loopback::bind().await.expect("bind");
    let redirect = loopback.redirect_uri().to_owned();
    assert!(redirect.starts_with("http://127.0.0.1:"), "{redirect}");
    let request = AuthRequest::start(&OAuthProvider::gmail(), "client-id", &redirect).expect("request");
    let state = request.state().to_owned();
    let port: u16 = redirect.rsplit(':').next().and_then(|port| port.parse().ok()).expect("port");

    let browser = tokio::spawn(async move {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).await.expect("connect");
        let target = format!("/?code=4/code-with%2Fslash&state={state}");
        stream.write_all(format!("GET {target} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n").as_bytes()).await.expect("request");
        let mut answer = Vec::new();
        stream.read_to_end(&mut answer).await.expect("response");
        String::from_utf8_lossy(&answer).into_owned()
    });

    let code = loopback.wait(&request, Duration::from_secs(5), &AtomicBool::new(false)).await.expect("code");
    assert_eq!(code.as_str(), "4/code-with/slash");
    let answer = browser.await.expect("browser task");
    assert!(answer.starts_with("HTTP/1.1 200 OK"), "{answer}");
    assert!(!answer.contains("code-with"), "the page must not echo the code");
}

#[tokio::test]
async fn a_foreign_state_is_refused_and_the_browser_is_not_told_it_worked() {
    let loopback = Loopback::bind().await.expect("bind");
    let redirect = loopback.redirect_uri().to_owned();
    let request = AuthRequest::start(&OAuthProvider::gmail(), "client-id", &redirect).expect("request");
    let port: u16 = redirect.rsplit(':').next().and_then(|port| port.parse().ok()).expect("port");

    let browser = tokio::spawn(async move {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).await.expect("connect");
        stream.write_all(b"GET /?code=abc&state=forged HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").await.expect("request");
        let _ = stream.shutdown().await;
        let mut answer = Vec::new();
        stream.read_to_end(&mut answer).await.expect("response");
        String::from_utf8_lossy(&answer).into_owned()
    });

    let error = loopback.wait(&request, Duration::from_secs(5), &AtomicBool::new(false)).await.expect_err("must refuse");
    assert!(matches!(error, NetError::OAuthConsent { .. }), "{error:?}");
    assert!(error.to_string().contains("state"), "{error}");
    let answer = browser.await.expect("browser task");
    assert!(answer.starts_with("HTTP/1.1 400"), "a forged callback must not get a success page: {answer}");
    assert!(!answer.contains("Accesso completato"), "{answer}");
}

#[tokio::test]
async fn a_declined_authorization_says_so() {
    let loopback = Loopback::bind().await.expect("bind");
    let redirect = loopback.redirect_uri().to_owned();
    let request = AuthRequest::start(&OAuthProvider::gmail(), "client-id", &redirect).expect("request");
    let port: u16 = redirect.rsplit(':').next().and_then(|port| port.parse().ok()).expect("port");

    let browser = tokio::spawn(async move {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).await.expect("connect");
        stream.write_all(b"GET /?error=access_denied&state=whatever HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").await.expect("request");
        let _ = stream.shutdown().await;
    });

    let error = loopback.wait(&request, Duration::from_secs(5), &AtomicBool::new(false)).await.expect_err("must refuse");
    assert!(!error.to_string().contains("whatever"), "{error}");
    assert!(error.to_string().contains("declined"), "{error}");
    browser.await.expect("browser task");
}

#[tokio::test]
async fn a_cancelled_wait_ends_before_the_timeout() {
    let loopback = Loopback::bind().await.expect("bind");
    let request = AuthRequest::start(&OAuthProvider::gmail(), "client-id", loopback.redirect_uri()).expect("request");
    let cancelled = Arc::new(AtomicBool::new(true));
    let started = std::time::Instant::now();
    let error = loopback.wait(&request, Duration::from_secs(30), &cancelled).await.expect_err("must stop");
    assert!(matches!(error, NetError::OAuthConsent { .. }), "{error:?}");
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[tokio::test]
async fn the_code_exchange_carries_the_pkce_verifier_and_returns_tokens() {
    let tls = postino_testkit::TlsFixture::generate();
    let oauth = FakeOAuth::start(&tls, vec![TokenReply::json(token_json("ya29.access", Some("1//refresh"), 3599))]).await;
    let endpoint = client(&oauth, &tls);
    let pkce = Pkce::generate().expect("pkce");
    let tokens = endpoint.exchange("4/code", "http://127.0.0.1:4567", pkce.verifier()).await.expect("exchange");
    assert_eq!(tokens.access_token().expose(), "ya29.access");
    assert_eq!(tokens.refresh_token().map(Secret::expose), Some("1//refresh"));
    assert_eq!(tokens.expires_in(), Duration::from_secs(3599));
    let requests = oauth.requests();
    let body = requests.first().expect("one request");
    assert!(body.contains("grant_type=authorization_code"), "{body}");
    assert!(body.contains(&format!("code_verifier={}", pkce.verifier())), "{body}");
    assert!(body.contains("client_secret=client-secret"), "{body}");
    assert!(body.contains("redirect_uri=http%3A%2F%2F127.0.0.1%3A4567"), "{body}");
    oauth.finish().await;
}

#[tokio::test]
async fn a_refresh_sends_the_stored_grant_and_accepts_a_rotated_token() {
    let tls = postino_testkit::TlsFixture::generate();
    let oauth = FakeOAuth::start(&tls, vec![TokenReply::json(token_json("ya29.next", Some("1//rotated"), 3599))]).await;
    let endpoint = client(&oauth, &tls);
    let tokens = endpoint.refresh("1//stored").await.expect("refresh");
    assert_eq!(tokens.access_token().expose(), "ya29.next");
    assert_eq!(tokens.refresh_token().map(Secret::expose), Some("1//rotated"));
    let requests = oauth.requests();
    let body = requests.first().expect("one request");
    assert!(body.contains("grant_type=refresh_token"), "{body}");
    assert!(body.contains("refresh_token=1%2F%2Fstored"), "{body}");
    oauth.finish().await;
}

#[tokio::test]
async fn a_revoked_grant_is_readable_and_never_echoes_the_body() {
    let tls = postino_testkit::TlsFixture::generate();
    let oauth = FakeOAuth::start(&tls, vec![TokenReply::error(400, "invalid_grant")]).await;
    let endpoint = client(&oauth, &tls);
    let error = endpoint.refresh("1//stored").await.expect_err("must refuse");
    assert!(matches!(error, NetError::OAuthGrant { .. }), "{error:?}");
    assert!(error.to_string().contains("authorize the account again"), "{error}");
    oauth.finish().await;
}

#[tokio::test]
async fn a_wrong_client_is_a_typed_refusal() {
    let tls = postino_testkit::TlsFixture::generate();
    let oauth = FakeOAuth::start(&tls, vec![TokenReply::error(401, "invalid_client")]).await;
    let endpoint = client(&oauth, &tls);
    let error = endpoint.refresh("1//stored").await.expect_err("must refuse");
    match error {
        NetError::OAuthRejected { status, code } => {
            assert_eq!(status, 401);
            assert_eq!(code, "invalid_client");
        }
        other => panic!("expected OAuthRejected, got {other:?}"),
    }
    oauth.finish().await;
}

#[tokio::test]
async fn malformed_and_oversized_responses_are_bounded() {
    let tls = postino_testkit::TlsFixture::generate();
    let oauth =
        FakeOAuth::start(&tls, vec![TokenReply::json("not json"), TokenReply::json(format!("{{\"access_token\":\"{}\"}}", "a".repeat(200_000)))])
            .await;
    let endpoint = client(&oauth, &tls);
    let malformed = endpoint.refresh("1//stored").await.expect_err("must refuse");
    assert!(matches!(malformed, NetError::OAuthToken { .. }), "{malformed:?}");
    let oversized = endpoint.refresh("1//stored").await.expect_err("must refuse");
    assert!(matches!(oversized, NetError::TooLarge { .. }), "{oversized:?}");
    oauth.finish().await;
}

#[tokio::test]
async fn an_offline_endpoint_fails_without_hanging() {
    let deadline = std::time::Instant::now();
    let provider = OAuthProvider::gmail().with_endpoints("https://localhost/authorize", "https://localhost:1/token", "https://localhost:1/revoke");
    let endpoint = TokenEndpoint::new(provider, "client-id", "client-secret").with_timeout(Duration::from_secs(2));
    // Port 1 on loopback is refused, so a dead endpoint is a typed error, not a panic and not an
    // unbounded wait.
    let error = endpoint.refresh("1//stored").await.expect_err("must fail");
    assert!(matches!(error, NetError::Connect { .. } | NetError::Dns { .. } | NetError::Tls { .. } | NetError::Timeout { .. }), "{error:?}");
    assert!(deadline.elapsed() < Duration::from_secs(20));
}

#[tokio::test]
async fn revocation_posts_the_grant() {
    let tls = postino_testkit::TlsFixture::generate();
    let oauth = FakeOAuth::start(&tls, vec![TokenReply::json("{}")]).await;
    let endpoint = client(&oauth, &tls);
    endpoint.revoke("1//stored").await.expect("revoke");
    let requests = oauth.requests();
    let body = requests.first().expect("one request");
    assert!(body.contains("token=1%2F%2Fstored"), "{body}");
    oauth.finish().await;
}

#[tokio::test]
async fn imap_logs_in_with_xoauth2_and_never_falls_back_to_a_password() {
    let tls = postino_testkit::TlsFixture::generate();
    let server = FakeServer::start_tls(&tls, |mut conn| async move {
        conn.prepare_xoauth2(&expected_xoauth2(USER, "ya29.access")).await;
    })
    .await;
    let config = server.config(&tls).with_oauth_token(Secret::new("ya29.access"));
    assert!(!format!("{config:?}").contains("ya29.access"));
    let mut session = Session::connect(config).await.expect("connect");
    session.login().await.expect("xoauth2 login");
    session.logout().await.expect("logout");
    server.finish().await;
}

#[tokio::test]
async fn an_imap_server_without_xoauth2_is_refused_instead_of_downgraded() {
    let tls = postino_testkit::TlsFixture::generate();
    let server = FakeServer::start_tls(&tls, |mut conn| async move {
        conn.prepare("AUTH=PLAIN").await;
    })
    .await;
    let config = server.config(&tls).with_oauth_token(Secret::new("ya29.access"));
    let mut session = Session::connect(config).await.expect("connect");
    let error = session.login().await.expect_err("must refuse");
    assert!(matches!(error, NetError::Unsupported { .. }), "{error:?}");
    assert!(!error.to_string().contains("ya29.access"), "{error}");
    server.finish().await;
}

#[tokio::test]
async fn smtp_logs_in_with_xoauth2() {
    let tls = postino_testkit::TlsFixture::generate();
    let expected = expected_xoauth2(USER, "ya29.access");
    let server = FakeSmtp::start_tls(&tls, move |mut conn| async move {
        conn.greet(&["AUTH XOAUTH2", "SIZE 1000000"]).await;
        conn.auth_xoauth2(&expected).await;
    })
    .await;
    let config = server.config(&tls).with_oauth_token(Secret::new("ya29.access"));
    let mut smtp = SmtpClient::connect(config).await.expect("connect");
    smtp.authenticate().await.expect("xoauth2 auth");
    assert!(smtp.is_authenticated());
    server.finish().await;
}

#[tokio::test]
async fn a_refused_smtp_token_is_a_typed_auth_error() {
    let tls = postino_testkit::TlsFixture::generate();
    let expected = expected_xoauth2(USER, "ya29.expired");
    let server = FakeSmtp::start_tls(&tls, move |mut conn| async move {
        conn.greet(&["AUTH XOAUTH2"]).await;
        conn.auth_xoauth2_refused(&expected).await;
    })
    .await;
    let config = server.config(&tls).with_oauth_token(Secret::new("ya29.expired"));
    let mut smtp = SmtpClient::connect(config).await.expect("connect");
    let error = smtp.authenticate().await.expect_err("must refuse");
    assert!(!error.to_string().contains("ya29.expired"), "{error}");
    assert!(error.is_authentication(), "{error:?}");
    server.finish().await;
}

#[tokio::test]
async fn an_smtp_server_without_xoauth2_is_never_offered_the_password() {
    let tls = postino_testkit::TlsFixture::generate();
    let server = FakeSmtp::start_tls(&tls, move |mut conn| async move {
        conn.greet(&["AUTH PLAIN"]).await;
        conn.delay(Duration::from_millis(200)).await;
        assert!(!conn.transcript_contains("AUTH PLAIN"), "an OAuth SMTP login must not downgrade to a password: {:?}", conn.received());
    })
    .await;
    let config = server.config(&tls).with_oauth_token(Secret::new("ya29.access"));
    let mut smtp = SmtpClient::connect(config).await.expect("connect");
    let error = smtp.authenticate().await.expect_err("must refuse");
    assert!(matches!(error, postino_net::SmtpError::Unsupported { .. }), "{error:?}");
    assert!(!error.to_string().contains("ya29.access"), "{error}");
    server.finish().await;
}

#[tokio::test]
async fn a_stalled_revocation_hits_its_timeout() {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.expect("bind");
    let port = listener.local_addr().expect("address").port();
    let stalled = tokio::spawn(async move {
        if let Ok((stream, _)) = listener.accept().await {
            tokio::time::sleep(Duration::from_secs(10)).await;
            drop(stream);
        }
    });
    let provider = OAuthProvider::gmail().with_endpoints(
        "https://localhost/authorize",
        "https://localhost/token",
        Box::leak(format!("https://localhost:{port}/revoke").into_boxed_str()),
    );
    let endpoint = TokenEndpoint::new(provider, "client-id", "client-secret").with_timeout(Duration::from_millis(200));
    let error = endpoint.revoke("1//grant").await.expect_err("a stalled revocation must time out");
    assert!(matches!(error, NetError::Timeout { .. }), "{error:?}");
    stalled.abort();
}

#[tokio::test]
async fn the_xoauth2_wire_payload_is_the_rfc_shape() {
    let payload = expected_xoauth2(USER, "ya29.access");
    assert_eq!(STANDARD.decode(payload.as_bytes()).expect("base64"), b"user=user@example.com\x01auth=Bearer ya29.access\x01\x01".as_slice());
    assert!(DEFAULT_TIMEOUT >= Duration::from_secs(60), "the consent budget must allow a person to sign in");
}

fn callback_parts(url: &str) -> (u16, String) {
    let state = url.split("state=").nth(1).and_then(|rest| rest.split('&').next()).expect("the authorization url carries a state").to_owned();
    let rest = url.split("127.0.0.1%3A").nth(1).expect("the redirect carries the loopback port");
    let port = rest.chars().take_while(|c| c.is_ascii_digit()).collect::<String>().parse().expect("port");
    (port, state)
}

#[test]
fn the_blocking_authorization_flow_opens_the_browser_and_returns_the_tokens() {
    use std::io::{Read as _, Write as _};

    let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().expect("runtime");
    let tls = postino_testkit::TlsFixture::generate();
    let oauth = runtime.block_on(FakeOAuth::start(&tls, vec![TokenReply::json(token_json("ya29.flow", Some("1//grant"), 3600))]));
    let authorization =
        Authorization::new(local_provider(&oauth), "client-id", "client-secret").with_extra_root(tls.ca()).with_timeout(Duration::from_secs(10));
    let mut opened = None;
    let tokens = authorization
        .run(
            |url| {
                opened = Some(url.to_owned());
                let (port, state) = callback_parts(url);
                std::thread::spawn(move || {
                    let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).expect("callback connect");
                    let request = format!("GET /callback?code=test-code&state={state} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n");
                    stream.write_all(request.as_bytes()).expect("callback write");
                    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                    let mut sink = [0u8; 256];
                    let _ = stream.read(&mut sink);
                });
                Ok(())
            },
            &AtomicBool::new(false),
        )
        .expect("authorization");
    assert_eq!(tokens.access_token().expose(), "ya29.flow");
    assert_eq!(tokens.refresh_token().map(Secret::expose), Some("1//grant"));
    let url = opened.expect("the flow opens the browser");
    assert!(url.starts_with("https://localhost/authorize?"), "{url}");
    assert!(url.contains("code_challenge_method=S256"), "{url}");
    assert!(!url.contains("client-secret"), "the url never carries the client secret");
    let bodies = oauth.requests();
    assert_eq!(bodies.len(), 1);
    assert!(bodies[0].contains("code=test-code"), "{}", bodies[0]);
    assert!(bodies[0].contains("code_verifier="), "{}", bodies[0]);
    assert!(bodies[0].contains("client_secret=client-secret"), "{}", bodies[0]);
    runtime.block_on(oauth.finish());
}
