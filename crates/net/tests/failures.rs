//! Timeouts and abrupt closes at every phase: always a typed error, never a panic or a hang.

mod support;

use std::time::Duration;

use postino_net::{NetError, Session, UidSet};
use support::{FakeServer, TlsFixture};

const SHORT: Duration = Duration::from_millis(250);

#[tokio::test]
async fn greeting_timeout() {
    let tls = TlsFixture::generate();
    let server = FakeServer::start_tls(&tls, |mut conn| async move {
        conn.delay(Duration::from_secs(3)).await;
    })
    .await;
    let error = Session::connect(server.config(&tls).with_timeout(SHORT)).await.expect_err("no greeting");
    match error {
        NetError::Timeout { phase, .. } => assert_eq!(phase, "greeting"),
        other => panic!("expected Timeout, got {other:?}"),
    }
    drop(server);
}

#[tokio::test]
async fn capability_timeout() {
    let tls = TlsFixture::generate();
    let server = FakeServer::start_tls(&tls, |mut conn| async move {
        conn.send_line("* OK ready").await;
        conn.expect("A0001 CAPABILITY").await;
        conn.delay(Duration::from_secs(3)).await;
    })
    .await;
    let error = Session::connect(server.config(&tls).with_timeout(SHORT)).await.expect_err("no capability");
    match error {
        NetError::Timeout { phase, .. } => assert_eq!(phase, "capability"),
        other => panic!("expected Timeout, got {other:?}"),
    }
    drop(server);
}

#[tokio::test]
async fn tls_handshake_timeout() {
    let tls = TlsFixture::generate();
    let server = FakeServer::start(|mut conn| async move {
        conn.delay(Duration::from_secs(3)).await;
    })
    .await;
    let config = postino_net::ImapConfig::new(
        "localhost",
        server.port(),
        postino_net::TlsMode::Implicit,
        support::USER,
        postino_net::Secret::new(support::PASSWORD),
    )
    .with_timeout(SHORT)
    .with_extra_root(tls.ca());
    let error = Session::connect(config).await.expect_err("the server never speaks TLS");
    match error {
        NetError::Timeout { phase, .. } => assert_eq!(phase, "tls"),
        other => panic!("expected Timeout, got {other:?}"),
    }
    drop(server);
}

#[tokio::test]
async fn abrupt_close_at_every_phase() {
    let phases = ["greeting", "login", "select", "fetch", "store", "noop"];
    for phase in phases {
        let tls = TlsFixture::generate();
        let server = FakeServer::start_tls(&tls, move |mut conn| async move {
            match phase {
                "greeting" => {}
                "login" => {
                    conn.send_line("* OK ready").await;
                    conn.expect("A0001 CAPABILITY").await;
                    conn.send_line("* CAPABILITY IMAP4rev1").await;
                    conn.send_line("A0001 OK done").await;
                }
                _ => {
                    conn.prepare("").await;
                    conn.login_ok().await;
                    if phase != "select" && phase != "noop" {
                        conn.expect("A0003 SELECT \"INBOX\"").await;
                        conn.send_line("* 0 EXISTS").await;
                        conn.send_line("* OK [UIDVALIDITY 1] ok").await;
                        conn.send_line("A0003 OK selected").await;
                    }
                }
            }
            // Return: the socket closes without answering the pending command.
        })
        .await;

        let mut session = if phase == "greeting" {
            let error = Session::connect(server.config(&tls)).await.expect_err("closed during greeting");
            assert_eq!(closed_phase(&error), "greeting");
            server.finish().await;
            continue;
        } else {
            Session::connect(server.config(&tls)).await.expect("connect")
        };
        match phase {
            "greeting" => {}
            "login" => {
                let error = session.login().await.expect_err("closed during login");
                assert_eq!(closed_phase(&error), "login", "{error:?} for {phase}");
            }
            "select" => {
                session.login().await.expect("login");
                let error = session.select("INBOX").await.expect_err("closed during select");
                assert_eq!(closed_phase(&error), "select", "{error:?} for {phase}");
            }
            "fetch" => {
                session.login().await.expect("login");
                session.select("INBOX").await.expect("select");
                let error = session.fetch_envelopes(UidSet::single(1).expect("uid")).await.expect_err("closed during fetch");
                assert_eq!(closed_phase(&error), "fetch", "{error:?} for {phase}");
            }
            "store" => {
                session.login().await.expect("login");
                session.select("INBOX").await.expect("select");
                let error = session
                    .store_flags(UidSet::single(1).expect("uid"), &[postino_net::Flag::Seen], postino_net::FlagMode::Add)
                    .await
                    .expect_err("closed during store");
                assert_eq!(closed_phase(&error), "store", "{error:?} for {phase}");
            }
            "noop" => {
                session.login().await.expect("login");
                let error = session.noop().await.expect_err("closed during noop");
                assert_eq!(closed_phase(&error), "noop", "{error:?} for {phase}");
            }
            other => panic!("unknown phase {other}"),
        }
        drop(session);
        server.finish().await;
    }
}

fn closed_phase(error: &NetError) -> &'static str {
    match error {
        NetError::Closed { phase } => phase,
        other => panic!("expected Closed, got {other:?}"),
    }
}

#[tokio::test]
async fn a_timeout_closes_the_session_so_the_next_command_fails_fast() {
    let tls = TlsFixture::generate();
    let server = FakeServer::start_tls(&tls, |mut conn| async move {
        conn.prepare_sasl().await;
        conn.expect("A0003 LIST \"\" \"*\"").await;
        conn.delay(Duration::from_secs(3)).await;
    })
    .await;

    let mut session = Session::connect(server.config(&tls).with_timeout(SHORT)).await.expect("connect");
    session.login().await.expect("login");
    let error = session.list().await.expect_err("the server never answers LIST");
    assert!(matches!(error, NetError::Timeout { .. }), "{error:?}");
    let error = session.noop().await.expect_err("the session is closed");
    assert!(matches!(error, NetError::Closed { .. }), "{error:?}");
    drop(server);
}

#[tokio::test]
async fn logout_on_a_broken_connection_is_not_an_error() {
    let tls = TlsFixture::generate();
    let server = FakeServer::start_tls(&tls, |mut conn| async move {
        conn.prepare_sasl().await;
    })
    .await;
    let mut session = Session::connect(server.config(&tls)).await.expect("connect");
    session.login().await.expect("login");
    server.finish().await;
    session.logout().await.expect("logout is best effort");
}

#[tokio::test]
async fn connecting_to_a_closed_port_is_a_typed_error() {
    let tls = TlsFixture::generate();
    // Bind and immediately drop the listener so the port is almost certainly closed.
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.expect("bind");
    let port = listener.local_addr().expect("addr").port();
    drop(listener);
    let config = postino_net::ImapConfig::new("127.0.0.1", port, postino_net::TlsMode::Implicit, support::USER, postino_net::Secret::new("x"))
        .with_timeout(SHORT)
        .with_extra_root(tls.ca());
    let error = Session::connect(config).await.expect_err("nothing listens there");
    assert!(matches!(error, NetError::Connect { .. } | NetError::Timeout { .. }), "{error:?}");
}

#[tokio::test]
async fn an_unresolvable_host_is_a_dns_error() {
    let tls = TlsFixture::generate();
    let config =
        postino_net::ImapConfig::new("no-such-host.invalid", 993, postino_net::TlsMode::Implicit, support::USER, postino_net::Secret::new("x"))
            .with_timeout(Duration::from_secs(5))
            .with_extra_root(tls.ca());
    let error = Session::connect(config).await.expect_err("the name does not resolve");
    assert!(matches!(error, NetError::Dns { .. }), "{error:?}");
}
