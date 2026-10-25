//! IDLE: one cycle, continuation, events, DONE and the ways a server can end it.

use std::time::Duration;

use postino_net::{IdleEvent, NetError, Session};
use postino_testkit::{FakeServer, TlsFixture, expected_sasl, idle_continuation};

async fn selected_session(server: &FakeServer, tls: &TlsFixture) -> Session {
    selected_session_with(server, tls, Duration::from_secs(5)).await
}

async fn selected_session_with(server: &FakeServer, tls: &TlsFixture, timeout: Duration) -> Session {
    let mut session = Session::connect(server.config(tls).with_timeout(timeout)).await.expect("connect");
    session.login().await.expect("login");
    session.select("INBOX").await.expect("select");
    session
}

#[tokio::test]
async fn idle_collects_events_then_done_and_tagged_ok() {
    let tls = TlsFixture::generate();
    let server = FakeServer::start_tls(&tls, |mut conn| async move {
        conn.prepare_sasl().await;
        conn.expect("A0003 SELECT \"INBOX\"").await;
        conn.send_line("* 0 EXISTS").await;
        conn.send_line("* OK [UIDVALIDITY 1] ok").await;
        conn.send_line("A0003 OK selected").await;
        conn.expect("A0004 IDLE").await;
        conn.send_line(&idle_continuation("idling")).await;
        conn.delay(Duration::from_millis(20)).await;
        conn.send_line("* 3 EXISTS").await;
        conn.send_line("* 1 EXPUNGE").await;
        conn.send_line("* 2 RECENT").await;
        conn.expect("DONE").await;
        conn.send_line("A0004 OK IDLE terminated").await;
    })
    .await;

    let mut session = selected_session(&server, &tls).await;
    let events = session.idle(Duration::from_secs(2)).await.expect("idle");
    assert_eq!(events, vec![IdleEvent::Exists(3), IdleEvent::Expunge(1), IdleEvent::Recent(2)]);
    session.logout().await.expect("logout");
    server.finish().await;
}

#[tokio::test]
async fn idle_with_no_events_waits_then_sends_done() {
    let tls = TlsFixture::generate();
    let server = FakeServer::start_tls(&tls, |mut conn| async move {
        conn.prepare_sasl().await;
        conn.expect("A0003 SELECT \"INBOX\"").await;
        conn.send_line("* 0 EXISTS").await;
        conn.send_line("* OK [UIDVALIDITY 1] ok").await;
        conn.send_line("A0003 OK selected").await;
        conn.expect("A0004 IDLE").await;
        conn.send_line("+ idling").await;
        conn.expect("DONE").await;
        conn.send_line("A0004 OK IDLE terminated").await;
        conn.expect("A0005 NOOP").await;
        conn.send_line("A0005 OK done").await;
    })
    .await;

    let mut session = selected_session(&server, &tls).await;
    let started = std::time::Instant::now();
    let events = session.idle(Duration::from_millis(150)).await.expect("idle");
    assert!(events.is_empty());
    assert!(started.elapsed() >= Duration::from_millis(100), "the client must actually wait");
    session.noop().await.expect("a quiet expiry does not close the session");
    session.logout().await.expect("logout");
    server.finish().await;
}

#[tokio::test]
async fn idle_duration_zero_sends_done_immediately() {
    let tls = TlsFixture::generate();
    let server = FakeServer::start_tls(&tls, |mut conn| async move {
        conn.prepare_sasl().await;
        conn.expect("A0003 SELECT \"INBOX\"").await;
        conn.send_line("* 0 EXISTS").await;
        conn.send_line("* OK [UIDVALIDITY 1] ok").await;
        conn.send_line("A0003 OK selected").await;
        conn.expect("A0004 IDLE").await;
        conn.send_line("+ idling").await;
        conn.expect("DONE").await;
        conn.send_line("A0004 OK IDLE terminated").await;
    })
    .await;

    let mut session = selected_session(&server, &tls).await;
    let events = session.idle(Duration::ZERO).await.expect("idle");
    assert!(events.is_empty());
    session.logout().await.expect("logout");
    server.finish().await;
}

#[tokio::test]
async fn a_disconnect_during_idle_is_a_closed_error() {
    let tls = TlsFixture::generate();
    let server = FakeServer::start_tls(&tls, |mut conn| async move {
        conn.prepare_sasl().await;
        conn.expect("A0003 SELECT \"INBOX\"").await;
        conn.send_line("* 0 EXISTS").await;
        conn.send_line("* OK [UIDVALIDITY 1] ok").await;
        conn.send_line("A0003 OK selected").await;
        conn.expect("A0004 IDLE").await;
        conn.send_line("+ idling").await;
        // close without DONE or a tagged response
    })
    .await;

    let mut session = selected_session(&server, &tls).await;
    let error = session.idle(Duration::from_secs(5)).await.expect_err("the server vanished");
    assert!(matches!(error, NetError::Closed { .. }), "{error:?}");
    server.finish().await;
}

#[tokio::test]
async fn a_bye_during_idle_is_an_event_not_a_panic() {
    let tls = TlsFixture::generate();
    let server = FakeServer::start_tls(&tls, |mut conn| async move {
        conn.prepare_sasl().await;
        conn.expect("A0003 SELECT \"INBOX\"").await;
        conn.send_line("* 0 EXISTS").await;
        conn.send_line("* OK [UIDVALIDITY 1] ok").await;
        conn.send_line("A0003 OK selected").await;
        conn.expect("A0004 IDLE").await;
        conn.send_line("+ idling").await;
        conn.send_line("* BYE server going down").await;
        conn.delay(Duration::from_millis(50)).await;
    })
    .await;

    let mut session = selected_session(&server, &tls).await;
    let events = session.idle(Duration::from_secs(5)).await.expect("idle returns the BYE");
    match events.last() {
        Some(IdleEvent::Bye(text)) => assert!(text.contains("going down"), "{text}"),
        other => panic!("expected a Bye event, got {other:?}"),
    }
    // The session knows it is closed and refuses further work.
    let error = session.noop().await.expect_err("closed session");
    assert!(matches!(error, NetError::Closed { .. }), "{error:?}");
    server.finish().await;
}

#[tokio::test]
async fn idle_requires_a_selected_mailbox() {
    let tls = TlsFixture::generate();
    let server = FakeServer::start_tls(&tls, |mut conn| async move {
        conn.prepare_sasl().await;
    })
    .await;

    let mut session = Session::connect(server.config(&tls)).await.expect("connect");
    session.login().await.expect("login");
    let error = session.idle(Duration::from_millis(10)).await.expect_err("no mailbox selected");
    assert!(matches!(error, NetError::Protocol { .. }), "{error:?}");
    drop(session);
    server.finish().await;
}

#[tokio::test]
async fn idle_without_the_capability_is_unsupported() {
    let tls = TlsFixture::generate();
    let server = FakeServer::start_tls(&tls, |mut conn| async move {
        conn.send_line("* OK ready").await;
        conn.expect("A0001 CAPABILITY").await;
        conn.send_line("* CAPABILITY IMAP4rev1 AUTH=PLAIN").await;
        conn.send_line("A0001 OK done").await;
        conn.expect("A0002 AUTHENTICATE PLAIN").await;
        conn.send_line("+ ").await;
        assert_eq!(conn.read_line().await, expected_sasl());
        // The tagged OK does not advertise IDLE, and the client adopts this list.
        conn.send_line("A0002 OK [CAPABILITY IMAP4rev1] logged in").await;
        conn.expect("A0003 SELECT \"INBOX\"").await;
        conn.send_line("* 0 EXISTS").await;
        conn.send_line("* OK [UIDVALIDITY 1] ok").await;
        conn.send_line("A0003 OK selected").await;
    })
    .await;

    let mut session = selected_session(&server, &tls).await;
    assert!(!session.capabilities().idle());
    let error = session.idle(Duration::from_millis(10)).await.expect_err("IDLE is not advertised");
    assert!(matches!(error, NetError::Unsupported { feature: "IDLE" }), "{error:?}");
    drop(session);
    server.finish().await;
}

/// Waiting for the `+` is a command exchange: a timeout there is not the idle window expiring.
#[tokio::test]
async fn continuation_timeout_closes_the_session() {
    let tls = TlsFixture::generate();
    let server = FakeServer::start_tls(&tls, |mut conn| async move {
        conn.prepare_sasl().await;
        conn.expect("A0003 SELECT \"INBOX\"").await;
        conn.send_line("* 0 EXISTS").await;
        conn.send_line("* OK [UIDVALIDITY 1] ok").await;
        conn.send_line("A0003 OK selected").await;
        conn.expect("A0004 IDLE").await;
        // Never send the continuation, and watch that the client does not keep talking.
        let next = tokio::time::timeout(Duration::from_millis(500), conn.read_line_opt()).await;
        assert!(matches!(next, Err(_) | Ok(None)), "the client sent {next:?} while waiting for +");
    })
    .await;

    let mut session = selected_session_with(&server, &tls, Duration::from_millis(250)).await;
    let error = session.idle(Duration::from_secs(5)).await.expect_err("no continuation");
    assert!(matches!(error, NetError::Timeout { phase: "idle", .. }), "{error:?}");
    let error = session.noop().await.expect_err("the session is closed");
    assert!(matches!(error, NetError::Closed { .. }), "{error:?}");
    drop(session);
    server.finish().await;
}

/// A timeout in the middle of a literal is a stalled connection, not the idle window ending.
#[tokio::test]
async fn partial_literal_timeout_is_not_a_clean_expiry() {
    let tls = TlsFixture::generate();
    let server = FakeServer::start_tls(&tls, |mut conn| async move {
        conn.prepare_sasl().await;
        conn.expect("A0003 SELECT \"INBOX\"").await;
        conn.send_line("* 0 EXISTS").await;
        conn.send_line("* OK [UIDVALIDITY 1] ok").await;
        conn.send_line("A0003 OK selected").await;
        conn.expect("A0004 IDLE").await;
        conn.send_line("+ idling").await;
        // Half a FETCH response, then silence: no DONE may be sent as if the wait had expired.
        conn.send("* 1 FETCH (UID 1 BODY[] {100}\r\nabc").await;
        let next = tokio::time::timeout(Duration::from_millis(600), conn.read_line_opt()).await;
        assert!(matches!(next, Err(_) | Ok(None)), "the client sent {next:?} mid-literal");
    })
    .await;

    let mut session = selected_session_with(&server, &tls, Duration::from_secs(2)).await;
    let error = session.idle(Duration::from_millis(300)).await.expect_err("half a literal is not an expiry");
    assert!(matches!(error, NetError::Timeout { phase: "idle", .. }), "{error:?}");
    let error = session.noop().await.expect_err("the session is closed");
    assert!(matches!(error, NetError::Closed { .. }), "{error:?}");
    drop(session);
    server.finish().await;
}

/// The `DONE` half of IDLE reads untagged responses too and must keep the same cap.
#[tokio::test]
async fn done_phase_enforces_max_untagged() {
    let tls = TlsFixture::generate();
    let server = FakeServer::start_tls(&tls, |mut conn| async move {
        conn.prepare_sasl().await;
        conn.expect("A0003 SELECT \"INBOX\"").await;
        conn.send_line("* 0 EXISTS").await;
        conn.send_line("* OK [UIDVALIDITY 1] ok").await;
        conn.send_line("A0003 OK selected").await;
        conn.expect("A0004 IDLE").await;
        conn.send_line("+ idling").await;
        conn.expect("DONE").await;
        for _ in 0..20 {
            conn.send_line("* 1 EXPUNGE").await;
        }
        conn.send_line("A0004 OK IDLE terminated").await;
    })
    .await;

    let mut config = server.config(&tls);
    config.limits.max_untagged = 10;
    let mut session = Session::connect(config).await.expect("connect");
    session.login().await.expect("login");
    session.select("INBOX").await.expect("select");
    let error = session.idle(Duration::ZERO).await.expect_err("20 responses is over the limit");
    assert!(matches!(error, NetError::TooLarge { what: "idle events", limit: 10 }), "{error:?}");
    let error = session.noop().await.expect_err("the session is closed");
    assert!(matches!(error, NetError::Closed { .. }), "{error:?}");
    drop(session);
    server.finish().await;
}
