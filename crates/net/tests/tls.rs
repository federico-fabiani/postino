//! TLS: implicit, STARTTLS, name verification and the plaintext traps.

mod support;

use std::time::Duration;

use postino_net::{NetError, Secret, Session, TlsMode};
use support::{FakeServer, TlsFixture, USER, expected_sasl};

#[tokio::test]
async fn implicit_tls_verifies_the_test_ca() {
    let tls = TlsFixture::generate();
    let server = FakeServer::start_tls(&tls, |mut conn| async move {
        conn.prepare("AUTH=PLAIN").await;
        conn.expect("A0002 AUTHENTICATE PLAIN").await;
        conn.send_line("+ ").await;
        assert_eq!(conn.read_line().await, expected_sasl());
        conn.send_line("A0002 OK logged in").await;
    })
    .await;

    // The CA is trusted only through `extra_roots`; the platform store does not know it.
    let mut session = Session::connect(server.config(&tls)).await.expect("implicit TLS");
    assert!(session.capabilities().imap4rev1());
    session.login().await.expect("login");
    session.logout().await.expect("logout");
    server.finish().await;
}

#[tokio::test]
async fn starttls_upgrades_then_asks_for_capabilities_again() {
    let tls = TlsFixture::generate();
    let acceptor = tls.acceptor();
    let server = FakeServer::start(move |mut conn| async move {
        conn.send_line("* OK plaintext greeting").await;
        conn.expect("A0001 CAPABILITY").await;
        conn.send_line("* CAPABILITY IMAP4rev1 STARTTLS").await;
        conn.send_line("A0001 OK done").await;
        conn.expect("A0002 STARTTLS").await;
        conn.send_line("A0002 OK begin TLS").await;
        assert!(conn.accept_tls(acceptor).await, "the client must start the handshake");
        conn.expect("A0003 CAPABILITY").await;
        conn.send_line("* CAPABILITY IMAP4rev1 AUTH=PLAIN").await;
        conn.send_line("A0003 OK done").await;
        conn.expect("A0004 AUTHENTICATE PLAIN").await;
        conn.send_line("+ ").await;
        assert_eq!(conn.read_line().await, expected_sasl());
        conn.send_line("A0004 OK logged in").await;
    })
    .await;

    let config = postino_net::ImapConfig::new("localhost", server.port(), TlsMode::StartTls, USER, Secret::new(support::PASSWORD))
        .with_timeout(Duration::from_secs(5))
        .with_extra_root(tls.ca());
    let mut session = Session::connect(config).await.expect("STARTTLS");
    assert!(session.capabilities().auth_plain());
    assert!(!session.capabilities().starttls(), "pre-TLS state is discarded");
    session.login().await.expect("login");
    session.logout().await.expect("logout");
    server.finish().await;
}

#[tokio::test]
async fn starttls_absent_is_refused_before_any_credential() {
    let tls = TlsFixture::generate();
    let server = FakeServer::start(|mut conn| async move {
        conn.send_line("* OK plaintext greeting").await;
        conn.expect("A0001 CAPABILITY").await;
        conn.send_line("* CAPABILITY IMAP4rev1 AUTH=PLAIN").await;
        conn.send_line("A0001 OK done").await;
        let extra = conn.read_line_opt().await;
        assert!(extra.is_none(), "the client sent {extra:?} without TLS");
    })
    .await;

    let config = postino_net::ImapConfig::new("localhost", server.port(), TlsMode::StartTls, USER, Secret::new(support::PASSWORD))
        .with_timeout(Duration::from_secs(2))
        .with_extra_root(tls.ca());
    let error = Session::connect(config).await.expect_err("STARTTLS is mandatory");
    assert!(matches!(error, NetError::Unsupported { feature: "STARTTLS" }), "{error:?}");
    server.finish().await;
}

#[tokio::test]
async fn a_certificate_for_another_name_is_refused() {
    let tls = TlsFixture::generate();
    let acceptor = tls.acceptor();
    let server = FakeServer::start(move |mut conn| async move {
        // The leaf is for `localhost`; the client dials 127.0.0.1, so the handshake must fail.
        let _ = conn.accept_tls(acceptor).await;
        conn.delay(Duration::from_millis(100)).await;
    })
    .await;

    let config = postino_net::ImapConfig::new("127.0.0.1", server.port(), TlsMode::Implicit, USER, Secret::new(support::PASSWORD))
        .with_timeout(Duration::from_secs(2))
        .with_extra_root(tls.ca());
    let error = Session::connect(config).await.expect_err("the name does not match");
    match &error {
        NetError::Tls { host, reason } => {
            assert_eq!(host, "127.0.0.1");
            assert!(!reason.is_empty());
            assert!(!reason.contains("BEGIN CERTIFICATE"));
        }
        other => panic!("expected Tls, got {other:?}"),
    }
    server.finish().await;
}

#[tokio::test]
async fn implicit_tls_against_a_plaintext_server_never_sends_a_command() {
    let tls = TlsFixture::generate();
    let server = FakeServer::start(|mut conn| async move {
        conn.send_line("* OK plaintext IMAP here").await;
        if let Some(line) = conn.read_line_opt().await {
            assert!(!line.contains("CAPABILITY") && !line.contains("A0001"), "plaintext command leaked: {line:?}");
        }
    })
    .await;

    let config = postino_net::ImapConfig::new("localhost", server.port(), TlsMode::Implicit, USER, Secret::new(support::PASSWORD))
        .with_timeout(Duration::from_secs(2))
        .with_extra_root(tls.ca());
    let error = Session::connect(config).await.expect_err("a plaintext server cannot speak implicit TLS");
    assert!(matches!(error, NetError::Tls { .. } | NetError::Closed { .. } | NetError::Protocol { .. }), "{error:?}");
    server.finish().await;
}

#[tokio::test]
async fn starttls_rejects_plaintext_pipelined_after_the_ok() {
    let tls = TlsFixture::generate();
    let acceptor = tls.acceptor();
    let server = FakeServer::start(move |mut conn| async move {
        conn.send_line("* OK plaintext greeting").await;
        conn.expect("A0001 CAPABILITY").await;
        conn.send_line("* CAPABILITY IMAP4rev1 STARTTLS").await;
        conn.send_line("A0001 OK done").await;
        conn.expect("A0002 STARTTLS").await;
        // A command injected before the handshake, on the same line group as the OK.
        conn.send("A0002 OK begin TLS\r\nA9999 CAPABILITY\r\n").await;
        let _ = conn.accept_tls(acceptor).await;
    })
    .await;

    let config = postino_net::ImapConfig::new("localhost", server.port(), TlsMode::StartTls, USER, Secret::new(support::PASSWORD))
        .with_timeout(Duration::from_secs(2))
        .with_extra_root(tls.ca());
    let error = Session::connect(config).await.expect_err("buffered plaintext after STARTTLS is refused");
    assert!(matches!(error, NetError::Protocol { .. }), "{error:?}");
    server.finish().await;
}
