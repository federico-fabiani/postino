//! SELECT and EXAMINE: status fields, read-only and the folder errors that matter.

use postino_net::{Flag, NetError, Session};
use postino_testkit::{FakeServer, TlsFixture};

#[tokio::test]
async fn select_reports_every_status_field() {
    let tls = TlsFixture::generate();
    let server = FakeServer::start_tls(&tls, |mut conn| async move {
        conn.prepare_sasl().await;
        conn.expect("A0003 SELECT \"INBOX\"").await;
        conn.send_line("* 12 EXISTS").await;
        conn.send_line("* 3 RECENT").await;
        conn.send_line("* OK [UIDVALIDITY 3857529045] UIDs valid").await;
        conn.send_line("* OK [UIDNEXT 4392] Predicted next UID").await;
        conn.send_line("* OK [UNSEEN 2] First unseen").await;
        conn.send_line("* OK [HIGHESTMODSEQ 12345] CONDSTORE").await;
        conn.send_line("* OK [PERMANENTFLAGS (\\Seen \\Deleted \\*) ] Permanent flags").await;
        conn.send_line("A0003 OK [READ-WRITE] SELECT completed").await;
    })
    .await;

    let mut session = Session::connect(server.config(&tls)).await.expect("connect");
    session.login().await.expect("login");
    let status = session.select("INBOX").await.expect("select");
    assert_eq!(status.exists, 12);
    assert_eq!(status.recent, 3);
    assert_eq!(status.uid_validity, 3_857_529_045);
    assert_eq!(status.uid_next, Some(4392));
    assert_eq!(status.unseen, Some(2));
    assert_eq!(status.highest_modseq, Some(12345));
    assert!(!status.read_only);
    assert!(status.permanent_flags.contains(&Flag::Seen));
    assert!(status.permanent_flags.contains(&Flag::Deleted));
    assert_eq!(session.selected_mailbox(), Some("INBOX"));
    session.logout().await.expect("logout");
    server.finish().await;
}

#[tokio::test]
async fn examine_is_read_only_even_without_a_code() {
    let tls = TlsFixture::generate();
    let server = FakeServer::start_tls(&tls, |mut conn| async move {
        conn.prepare_sasl().await;
        conn.expect("A0003 EXAMINE \"INBOX\"").await;
        conn.send_line("* 5 EXISTS").await;
        conn.send_line("* 0 RECENT").await;
        conn.send_line("* OK [UIDVALIDITY 7] UIDs valid").await;
        conn.send_line("A0003 OK EXAMINE completed").await;
    })
    .await;

    let mut session = Session::connect(server.config(&tls)).await.expect("connect");
    session.login().await.expect("login");
    let status = session.examine("INBOX").await.expect("examine");
    assert_eq!(status.exists, 5);
    assert!(status.read_only);
    assert_eq!(status.uid_validity, 7);
    session.logout().await.expect("logout");
    server.finish().await;
}

#[tokio::test]
async fn missing_uidvalidity_is_a_protocol_error() {
    let tls = TlsFixture::generate();
    let server = FakeServer::start_tls(&tls, |mut conn| async move {
        conn.prepare_sasl().await;
        conn.expect("A0003 SELECT \"INBOX\"").await;
        conn.send_line("* 1 EXISTS").await;
        conn.send_line("* 0 RECENT").await;
        conn.send_line("A0003 OK selected").await;
    })
    .await;

    let mut session = Session::connect(server.config(&tls)).await.expect("connect");
    session.login().await.expect("login");
    let error = session.select("INBOX").await.expect_err("UIDVALIDITY is required");
    assert!(matches!(error, NetError::Protocol { .. }), "{error:?}");
    assert_eq!(session.selected_mailbox(), None);
    drop(session);
    server.finish().await;
}

#[tokio::test]
async fn a_no_on_select_keeps_the_response_code_readable() {
    let tls = TlsFixture::generate();
    let server = FakeServer::start_tls(&tls, |mut conn| async move {
        conn.prepare_sasl().await;
        conn.expect("A0003 SELECT \"Missing\"").await;
        conn.send_line("A0003 NO [NONEXISTENT] Mailbox does not exist").await;
    })
    .await;

    let mut session = Session::connect(server.config(&tls)).await.expect("connect");
    session.login().await.expect("login");
    let error = session.select("Missing").await.expect_err("select must fail");
    match &error {
        NetError::ServerNo { command, reason } => {
            assert_eq!(*command, "SELECT");
            assert!(reason.contains("NONEXISTENT"), "{reason}");
            assert!(reason.contains("does not exist"), "{reason}");
        }
        other => panic!("expected ServerNo, got {other:?}"),
    }
    assert_eq!(session.selected_mailbox(), None);
    server.finish().await;
}

#[tokio::test]
async fn select_quotes_and_escapes_the_mailbox_name() {
    let tls = TlsFixture::generate();
    let server = FakeServer::start_tls(&tls, |mut conn| async move {
        conn.prepare_sasl().await;
        conn.expect("A0003 SELECT \"a \\\"quoted\\\" folder\"").await;
        conn.send_line("* 0 EXISTS").await;
        conn.send_line("* OK [UIDVALIDITY 1] ok").await;
        conn.send_line("A0003 OK selected").await;
    })
    .await;

    let mut session = Session::connect(server.config(&tls)).await.expect("connect");
    session.login().await.expect("login");
    session.select("a \"quoted\" folder").await.expect("select");
    session.logout().await.expect("logout");
    server.finish().await;
}

/// RFC 3501 reports a read-only mailbox with `OK [READ-ONLY]` on the tagged response. A client that
/// only looks at untagged responses misses it and offers writes the server will refuse.
#[tokio::test]
async fn select_honors_the_tagged_read_only_code() {
    let tls = TlsFixture::generate();
    let server = FakeServer::start_tls(&tls, |mut conn| async move {
        conn.prepare_sasl().await;
        conn.expect("A0003 SELECT \"Archive\"").await;
        conn.send_line("* 5 EXISTS").await;
        conn.send_line("* 0 RECENT").await;
        conn.send_line("* OK [UIDVALIDITY 9] UIDs valid").await;
        conn.send_line("A0003 OK [READ-ONLY] SELECT completed").await;
    })
    .await;

    let mut session = Session::connect(server.config(&tls)).await.expect("connect");
    session.login().await.expect("login");
    let status = session.select("Archive").await.expect("select");
    assert!(status.read_only, "the tagged READ-ONLY code was ignored");
    assert_eq!(status.uid_validity, 9);
    assert_eq!(status.exists, 5);
    session.logout().await.expect("logout");
    server.finish().await;
}
