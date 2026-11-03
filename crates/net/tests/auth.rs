//! Authentication paths: SASL PLAIN, LOGIN, failures, LOGINDISABLED and PREAUTH.

use postino_net::{NetError, Session};
use postino_testkit::{FakeServer, TlsFixture, expected_sasl};

#[tokio::test]
async fn sasl_plain_is_preferred_and_the_password_never_reaches_a_command_line() {
    let tls = TlsFixture::generate();
    let server = FakeServer::start_tls(&tls, |mut conn| async move {
        conn.prepare("AUTH=PLAIN IDLE").await;
        conn.expect("A0002 AUTHENTICATE PLAIN").await;
        conn.send_line("+ ").await;
        assert_eq!(conn.read_line().await, expected_sasl());
        conn.send_line("A0002 OK [CAPABILITY IMAP4rev1 IDLE] authenticated").await;
    })
    .await;

    let mut session = Session::connect(server.config(&tls)).await.expect("connect");
    session.login().await.expect("login");
    assert!(session.capabilities().idle(), "capabilities from the tagged OK are adopted");
    assert!(!session.capabilities().auth_plain(), "the server did not repeat AUTH=PLAIN");
    session.logout().await.expect("logout");
    server.finish().await;
}

#[tokio::test]
async fn login_failure_is_a_typed_auth_error() {
    let tls = TlsFixture::generate();
    let server = FakeServer::start_tls(&tls, |mut conn| async move {
        conn.prepare("").await;
        conn.expect("A0002 LOGIN \"user@example.com\" \"correct horse battery\"").await;
        conn.send_line("A0002 NO [AUTHENTICATIONFAILED] Invalid credentials").await;
    })
    .await;

    let mut session = Session::connect(server.config(&tls)).await.expect("connect");
    let error = session.login().await.expect_err("login must fail");
    match &error {
        NetError::Auth { reason } => assert!(reason.contains("AUTHENTICATIONFAILED"), "{reason}"),
        other => panic!("expected Auth, got {other:?}"),
    }
    assert!(!error.to_string().contains("correct horse"), "{error}");
    server.finish().await;
}

#[tokio::test]
async fn sasl_failure_is_a_typed_auth_error() {
    let tls = TlsFixture::generate();
    let server = FakeServer::start_tls(&tls, |mut conn| async move {
        conn.prepare("AUTH=PLAIN").await;
        conn.expect("A0002 AUTHENTICATE PLAIN").await;
        conn.send_line("+ ").await;
        assert_eq!(conn.read_line().await, expected_sasl());
        conn.send_line("A0002 NO [AUTHORIZATIONFAILED] nope").await;
    })
    .await;

    let mut session = Session::connect(server.config(&tls)).await.expect("connect");
    let error = session.login().await.expect_err("login must fail");
    assert!(matches!(error, NetError::Auth { .. }), "{error:?}");
    server.finish().await;
}

#[tokio::test]
async fn logindisabled_is_refused_before_any_credential_is_sent() {
    let tls = TlsFixture::generate();
    let server = FakeServer::start_tls(&tls, |mut conn| async move {
        conn.prepare("LOGINDISABLED").await;
    })
    .await;

    let mut session = Session::connect(server.config(&tls)).await.expect("connect");
    let error = session.login().await.expect_err("login must be refused");
    match &error {
        NetError::Auth { reason } => assert!(reason.contains("LOGINDISABLED"), "{reason}"),
        other => panic!("expected Auth, got {other:?}"),
    }
    drop(session);
    server.finish().await;
}

#[tokio::test]
async fn preauth_skips_login_entirely() {
    let tls = TlsFixture::generate();
    let server = FakeServer::start_tls(&tls, |mut conn| async move {
        conn.send_line("* PREAUTH [CAPABILITY IMAP4rev1] already authenticated").await;
        conn.expect("A0001 CAPABILITY").await;
        conn.send_line("* CAPABILITY IMAP4rev1 IDLE").await;
        conn.send_line("A0001 OK done").await;
        let extra = conn.read_line_opt().await;
        assert!(extra.is_none(), "PREAUTH must not send a login command, got {extra:?}");
    })
    .await;

    let mut session = Session::connect(server.config(&tls)).await.expect("connect");
    session.login().await.expect("login is a no-op after PREAUTH");
    drop(session);
    server.finish().await;
}

/// The SASL exchange has its own reads and writes; a timeout there must close the session so a
/// retry cannot send a second `AUTHENTICATE` into a stream that is missing its continuation.
#[tokio::test]
async fn sasl_continuation_timeout_closes_the_session() {
    let tls = TlsFixture::generate();
    let server = FakeServer::start_tls(&tls, |mut conn| async move {
        conn.send_line("* OK ready").await;
        conn.expect("A0001 CAPABILITY").await;
        conn.send_line("* CAPABILITY IMAP4rev1 AUTH=PLAIN").await;
        conn.send_line("A0001 OK done").await;
        conn.expect("A0002 AUTHENTICATE PLAIN").await;
        // No continuation; the client must not retry on this connection.
        let next = tokio::time::timeout(std::time::Duration::from_millis(700), conn.read_line_opt()).await;
        assert!(matches!(next, Err(_) | Ok(None)), "a retry reached the server: {next:?}");
        assert_eq!(
            conn.received().iter().filter(|line| line.contains("AUTHENTICATE")).count(),
            1,
            "exactly one AUTHENTICATE, got {:?}",
            conn.received()
        );
    })
    .await;

    let config = server.config(&tls).with_timeout(std::time::Duration::from_millis(250));
    let mut session = Session::connect(config).await.expect("connect");
    let error = session.login().await.expect_err("no challenge");
    assert!(matches!(error, NetError::Timeout { phase: "auth", .. }), "{error:?}");
    let error = session.login().await.expect_err("the session is closed");
    assert!(matches!(error, NetError::Closed { phase: "auth" }), "{error:?}");
    drop(session);
    server.finish().await;
}

#[tokio::test]
async fn id_names_the_client_once_the_server_advertises_it() {
    let payload = format!("(\"name\" \"Postino\" \"version\" \"{}\" \"vendor\" \"Postino\")", env!("CARGO_PKG_VERSION"));
    let tls = TlsFixture::generate();
    let server = FakeServer::start_tls(&tls, |mut conn| async move {
        conn.prepare_sasl_with("ID").await;
        conn.expect(&format!("A0003 ID {payload}")).await;
        conn.send_line("* ID (\"name\" \"Courier-IMAP\" \"version\" \"4.1\")").await;
        conn.send_line("A0003 OK done").await;
    })
    .await;

    let mut session = Session::connect(server.config(&tls)).await.expect("connect");
    session.login().await.expect("login");
    assert!(session.capabilities().id(), "the post-auth list is the one that counts");
    session.logout().await.expect("logout");
    server.finish().await;
}

#[tokio::test]
async fn a_refused_id_is_not_a_failed_login() {
    let tls = TlsFixture::generate();
    let server = FakeServer::start_tls(&tls, |mut conn| async move {
        conn.prepare_sasl_with("ID").await;
        conn.expect_prefix("A0003 ID (").await;
        conn.send_line("A0003 BAD unknown command").await;
    })
    .await;

    let mut session = Session::connect(server.config(&tls)).await.expect("connect");
    session.login().await.expect("ID is optional, the session must survive its refusal");
    session.logout().await.expect("logout");
    server.finish().await;
}
