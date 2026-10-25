//! FETCH parsing: literals, split reads, unsolicited responses, bounds and malformed replies.

mod support;

use postino_net::{NetError, Session, UidSet};
use support::{FakeServer, TlsFixture};

#[tokio::test]
async fn literal_split_across_reads_is_reassembled() {
    let tls = TlsFixture::generate();
    let server = FakeServer::start_tls(&tls, |mut conn| async move {
        conn.prepare_sasl().await;
        conn.expect("A0003 SELECT \"INBOX\"").await;
        conn.send_line("* 0 EXISTS").await;
        conn.send_line("* OK [UIDVALIDITY 1] ok").await;
        conn.send_line("A0003 OK selected").await;
        conn.expect("A0004 UID FETCH 1 (BODY.PEEK[])").await;
        let response = support::fetch_body_response(1, 1, b"Subject: split\r\n\r\nbody bytes");
        // Split the literal across two writes, with a pause in between.
        let (head, tail) = response.split_at(30);
        conn.send_bytes(head).await;
        conn.delay(std::time::Duration::from_millis(20)).await;
        conn.send_bytes(tail).await;
        conn.send_line("A0004 OK done").await;
    })
    .await;

    let mut session = Session::connect(server.config(&tls)).await.expect("connect");
    session.login().await.expect("login");
    session.select("INBOX").await.expect("select");
    let body = session.fetch_body(1).await.expect("body");
    assert_eq!(body, b"Subject: split\r\n\r\nbody bytes");
    session.logout().await.expect("logout");
    server.finish().await;
}

#[tokio::test]
async fn two_literals_in_one_response_are_parsed_in_order() {
    let tls = TlsFixture::generate();
    let server = FakeServer::start_tls(&tls, |mut conn| async move {
        conn.prepare_sasl().await;
        conn.expect("A0003 SELECT \"INBOX\"").await;
        conn.send_line("* 0 EXISTS").await;
        conn.send_line("* OK [UIDVALIDITY 1] ok").await;
        conn.send_line("A0003 OK selected").await;
        conn.expect("A0004 UID FETCH 7 (BODY.PEEK[])").await;
        conn.send("* 1 FETCH (UID 7 BODY[HEADER.FIELDS (DATE)] {9}\r\nDate: x\r\n BODY[] {10}\r\npayload123)\r\n").await;
        conn.send_line("A0004 OK done").await;
    })
    .await;

    let mut session = Session::connect(server.config(&tls)).await.expect("connect");
    session.login().await.expect("login");
    session.select("INBOX").await.expect("select");
    let body = session.fetch_body(7).await.expect("body");
    assert_eq!(body, b"payload123");
    session.logout().await.expect("logout");
    server.finish().await;
}

#[tokio::test]
async fn unsolicited_exists_does_not_confuse_the_fetch() {
    let tls = TlsFixture::generate();
    let server = FakeServer::start_tls(&tls, |mut conn| async move {
        conn.prepare_sasl().await;
        conn.expect("A0003 SELECT \"INBOX\"").await;
        conn.send_line("* 0 EXISTS").await;
        conn.send_line("* OK [UIDVALIDITY 1] ok").await;
        conn.send_line("A0003 OK selected").await;
        conn.expect("A0004 UID FETCH 3 (UID FLAGS INTERNALDATE RFC822.SIZE ENVELOPE)").await;
        conn.send_line("* 9 EXISTS").await;
        conn.send_line("* 1 FETCH (UID 3 FLAGS () INTERNALDATE \"01-Jan-2024 00:00:00 +0000\" RFC822.SIZE 10 ENVELOPE (NIL \"subject\" NIL NIL NIL NIL NIL NIL NIL NIL))")
            .await;
        conn.send_line("* 4 RECENT").await;
        conn.send_line("A0004 OK done").await;
    })
    .await;

    let mut session = Session::connect(server.config(&tls)).await.expect("connect");
    session.login().await.expect("login");
    session.select("INBOX").await.expect("select");
    let envelopes = session.fetch_envelopes(UidSet::single(3).expect("uid")).await.expect("fetch");
    assert_eq!(envelopes.len(), 1);
    assert_eq!(envelopes.first().map(|envelope| envelope.uid), Some(3));
    assert_eq!(envelopes.first().map(|envelope| envelope.subject.as_str()), Some("subject"));
    session.logout().await.expect("logout");
    server.finish().await;
}

#[tokio::test]
async fn an_oversized_literal_is_refused_before_it_is_allocated() {
    let tls = TlsFixture::generate();
    let server = FakeServer::start_tls(&tls, |mut conn| async move {
        conn.prepare_sasl().await;
        conn.expect("A0003 SELECT \"INBOX\"").await;
        conn.send_line("* 0 EXISTS").await;
        conn.send_line("* OK [UIDVALIDITY 1] ok").await;
        conn.send_line("A0003 OK selected").await;
        conn.expect("A0004 UID FETCH 1 (BODY.PEEK[])").await;
        conn.send_line("* 1 FETCH (UID 1 BODY[] {100}").await;
        conn.delay(std::time::Duration::from_secs(2)).await;
    })
    .await;

    let mut config = server.config(&tls);
    config.limits.max_literal = 8;
    let mut session = Session::connect(config).await.expect("connect");
    session.login().await.expect("login");
    session.select("INBOX").await.expect("select");
    let error = session.fetch_body(1).await.expect_err("the literal is over the limit");
    match error {
        NetError::TooLarge { what, limit } => {
            assert_eq!(what, "literal");
            assert_eq!(limit, 8);
        }
        other => panic!("expected TooLarge, got {other:?}"),
    }
    drop(session);
    server.finish().await;
}

#[tokio::test]
async fn a_malformed_fetch_is_a_protocol_error() {
    let tls = TlsFixture::generate();
    let server = FakeServer::start_tls(&tls, |mut conn| async move {
        conn.prepare_sasl().await;
        conn.expect("A0003 SELECT \"INBOX\"").await;
        conn.send_line("* 0 EXISTS").await;
        conn.send_line("* OK [UIDVALIDITY 1] ok").await;
        conn.send_line("A0003 OK selected").await;
        conn.expect("A0004 UID FETCH 1 (BODY.PEEK[])").await;
        conn.send_line("* 1 FETCH (UID 1").await;
        conn.send_line("A0004 OK done").await;
    })
    .await;

    let mut session = Session::connect(server.config(&tls)).await.expect("connect");
    session.login().await.expect("login");
    session.select("INBOX").await.expect("select");
    let error = session.fetch_body(1).await.expect_err("unterminated list");
    assert!(matches!(error, NetError::Protocol { .. }), "{error:?}");
    server.finish().await;
}

#[tokio::test]
async fn a_truncated_literal_is_a_closed_connection() {
    let tls = TlsFixture::generate();
    let server = FakeServer::start_tls(&tls, |mut conn| async move {
        conn.prepare_sasl().await;
        conn.expect("A0003 SELECT \"INBOX\"").await;
        conn.send_line("* 0 EXISTS").await;
        conn.send_line("* OK [UIDVALIDITY 1] ok").await;
        conn.send_line("A0003 OK selected").await;
        conn.expect("A0004 UID FETCH 1 (BODY.PEEK[])").await;
        conn.send("* 1 FETCH (UID 1 BODY[] {100}\r\nabc").await;
        // the handler returns, closing the socket mid-literal
    })
    .await;

    let mut session = Session::connect(server.config(&tls)).await.expect("connect");
    session.login().await.expect("login");
    session.select("INBOX").await.expect("select");
    let error = session.fetch_body(1).await.expect_err("the literal never completes");
    assert!(matches!(error, NetError::Closed { .. }), "{error:?}");
    server.finish().await;
}

#[tokio::test]
async fn fetching_a_body_for_uid_zero_is_refused_locally() {
    let tls = TlsFixture::generate();
    let server = FakeServer::start_tls(&tls, |mut conn| async move {
        conn.prepare_sasl().await;
    })
    .await;

    let mut session = Session::connect(server.config(&tls)).await.expect("connect");
    session.login().await.expect("login");
    let error = session.fetch_body(0).await.expect_err("UID 0 does not exist");
    assert!(matches!(error, NetError::Protocol { .. }), "{error:?}");
    drop(session);
    server.finish().await;
}

/// A bound error stops in the middle of a response. The session must say so, and the server must
/// see no further command, because a second read would start inside the abandoned literal.
#[tokio::test]
async fn a_bound_error_closes_the_session_and_stops_the_wire() {
    let tls = TlsFixture::generate();
    let server = FakeServer::start_tls(&tls, |mut conn| async move {
        conn.prepare_sasl().await;
        conn.expect("A0003 SELECT \"INBOX\"").await;
        conn.send_line("* 0 EXISTS").await;
        conn.send_line("* OK [UIDVALIDITY 1] ok").await;
        conn.send_line("A0003 OK selected").await;
        conn.expect("A0004 UID FETCH 1 (BODY.PEEK[])").await;
        // A literal over `max_literal`, with more responses queued behind it.
        conn.send_line("* 1 FETCH (UID 1 BODY[] {100}").await;
        conn.send_line("* 2 EXPUNGE").await;
        conn.send_line("A0004 OK done").await;
        // The client may not send anything else: wait for a command or for the socket to close.
        let next = tokio::time::timeout(std::time::Duration::from_millis(300), conn.read_line_opt()).await;
        assert!(matches!(next, Err(_) | Ok(None)), "the client kept talking after TooLarge: {next:?}");
    })
    .await;

    let mut config = server.config(&tls);
    config.limits.max_literal = 8;
    let mut session = Session::connect(config).await.expect("connect");
    session.login().await.expect("login");
    session.select("INBOX").await.expect("select");
    let error = session.fetch_body(1).await.expect_err("the literal is over the limit");
    assert!(matches!(error, NetError::TooLarge { what: "literal", .. }), "{error:?}");
    let error = session.noop().await.expect_err("the session is closed");
    assert!(matches!(error, NetError::Closed { phase: "noop" }), "{error:?}");
    drop(session);
    server.finish().await;
}

#[tokio::test]
async fn fetch_body_refuses_a_body_with_another_uid() {
    let tls = TlsFixture::generate();
    let server = FakeServer::start_tls(&tls, |mut conn| async move {
        conn.prepare_sasl().await;
        conn.expect("A0003 SELECT \"INBOX\"").await;
        conn.send_line("* 0 EXISTS").await;
        conn.send_line("* OK [UIDVALIDITY 1] ok").await;
        conn.send_line("A0003 OK selected").await;
        conn.expect("A0004 UID FETCH 1 (BODY.PEEK[])").await;
        conn.send_bytes(&support::fetch_body_response(1, 2, b"the wrong message")).await;
        conn.send_line("A0004 OK done").await;
    })
    .await;

    let mut session = Session::connect(server.config(&tls)).await.expect("connect");
    session.login().await.expect("login");
    session.select("INBOX").await.expect("select");
    let error = session.fetch_body(1).await.expect_err("UID 2 is not UID 1");
    match &error {
        NetError::Protocol { reason } => assert!(reason.contains("UID 2") && reason.contains("UID 1"), "{reason}"),
        other => panic!("expected Protocol, got {other:?}"),
    }
    // The body was consumed completely, but a lying server is a protocol failure: stop here.
    let error = session.noop().await.expect_err("closed");
    assert!(matches!(error, NetError::Closed { .. }), "{error:?}");
    server.finish().await;
}

#[tokio::test]
async fn fetch_envelopes_refuses_a_uid_outside_the_requested_set() {
    let tls = TlsFixture::generate();
    let server = FakeServer::start_tls(&tls, |mut conn| async move {
        conn.prepare_sasl().await;
        conn.expect("A0003 SELECT \"INBOX\"").await;
        conn.send_line("* 0 EXISTS").await;
        conn.send_line("* OK [UIDVALIDITY 1] ok").await;
        conn.send_line("A0003 OK selected").await;
        conn.expect("A0004 UID FETCH 5 (UID FLAGS INTERNALDATE RFC822.SIZE ENVELOPE)").await;
        conn.send_line("* 1 FETCH (UID 6 FLAGS () INTERNALDATE \"01-Jan-2024 00:00:00 +0000\" RFC822.SIZE 10 ENVELOPE (NIL \"wrong\" NIL NIL NIL NIL NIL NIL NIL NIL))")
            .await;
        conn.send_line("A0004 OK done").await;
    })
    .await;

    let mut session = Session::connect(server.config(&tls)).await.expect("connect");
    session.login().await.expect("login");
    session.select("INBOX").await.expect("select");
    let error = session.fetch_envelopes(UidSet::parse("5").expect("uid")).await.expect_err("UID 6 was not requested");
    match &error {
        NetError::Protocol { reason } => assert!(reason.contains("UID 6"), "{reason}"),
        other => panic!("expected Protocol, got {other:?}"),
    }
    server.finish().await;
}

/// A literal that arrived in the same socket read as its header must still be charged to the
/// response budget, or `max_response_bytes` bounds only the socket reads.
#[tokio::test]
async fn a_buffered_literal_counts_against_the_response_budget() {
    let tls = TlsFixture::generate();
    let server = FakeServer::start_tls(&tls, |mut conn| async move {
        conn.prepare_sasl().await;
        conn.expect("A0003 SELECT \"INBOX\"").await;
        conn.send_line("* 0 EXISTS").await;
        conn.send_line("* OK [UIDVALIDITY 1] ok").await;
        conn.send_line("A0003 OK selected").await;
        conn.expect("A0004 UID FETCH 1 (BODY.PEEK[])").await;
        // One write, so the literal is already in the client's read buffer when the line is parsed.
        conn.send_bytes(&support::fetch_body_response(1, 1, &[b'x'; 40])).await;
        conn.send_line("A0004 OK done").await;
    })
    .await;

    let mut config = server.config(&tls);
    // Every exchange before the fetch is well under this; the fetch line plus its 40-byte literal
    // is not.
    config.limits.max_response_bytes = 65;
    let mut session = Session::connect(config).await.expect("connect");
    session.login().await.expect("login");
    session.select("INBOX").await.expect("select");
    let error = session.fetch_body(1).await.expect_err("line plus literal is over 65 bytes");
    assert!(matches!(error, NetError::TooLarge { what: "response", limit: 65 }), "{error:?}");
    drop(session);
    server.finish().await;
}
