//! One full session, asserting every command byte the client writes.

use postino_net::{Flag, FlagMode, Session, UidSet, decode_utf7};
use postino_testkit::{FakeServer, PASSWORD, TlsFixture, USER, expected_sasl, list_line};

#[tokio::test]
async fn full_session_sends_the_documented_commands() {
    let tls = TlsFixture::generate();
    let server = FakeServer::start_tls(&tls, |mut conn| async move {
        conn.prepare("AUTH=PLAIN IDLE").await;

        // AUTHENTICATE PLAIN, with the password never in a command line.
        conn.expect("A0002 AUTHENTICATE PLAIN").await;
        conn.send_line("+ go ahead").await;
        assert_eq!(conn.read_line().await, expected_sasl());
        conn.send_line("A0002 OK [CAPABILITY IMAP4rev1 IDLE] logged in").await;

        conn.expect("A0003 LIST \"\" \"*\"").await;
        conn.send_line(&list_line(&["\\HasNoChildren", "\\Inbox"], Some('/'), "INBOX")).await;
        conn.send_line(&list_line(&["\\HasNoChildren", "\\Sent"], Some('/'), "Sent")).await;
        conn.send_line(&list_line(&["\\Noselect", "\\HasChildren"], Some('/'), "Archivio")).await;
        conn.send_line("A0003 OK done").await;

        conn.expect("A0004 SELECT \"INBOX\"").await;
        conn.send_line("* 2 EXISTS").await;
        conn.send_line("* 0 RECENT").await;
        conn.send_line("* OK [UIDVALIDITY 42] UIDs valid").await;
        conn.send_line("* OK [UIDNEXT 10] predicted next UID").await;
        conn.send_line("* OK [UNSEEN 1] first unseen").await;
        conn.send_line("* FLAGS (\\Seen \\Answered)").await;
        conn.send_line("* OK [PERMANENTFLAGS (\\Seen \\*)] limited").await;
        conn.send_line("A0004 OK [READ-WRITE] selected").await;

        conn.expect("A0005 UID FETCH 1:2 (UID FLAGS INTERNALDATE RFC822.SIZE ENVELOPE)").await;
        conn.send_line(concat!(
            "* 1 FETCH (UID 1 FLAGS (\\Seen) INTERNALDATE \"17-Jul-1996 02:44:25 -0700\" RFC822.SIZE 128 ",
            "ENVELOPE (\"Mon, 1 Jan 2024 10:00:00 +0000\" \"Ciao\" ",
            "((\"Giulia Ferri\" NIL \"giulia\" \"example.com\")) ",
            "((\"Giulia Ferri\" NIL \"giulia\" \"example.com\")) ",
            "((\"Giulia Ferri\" NIL \"giulia\" \"example.com\")) ",
            "((\"Me\" NIL \"me\" \"example.it\")) NIL NIL NIL \"<a@example.com>\"))"
        ))
        .await;
        conn.send_line(concat!(
            "* 2 FETCH (UID 2 FLAGS () INTERNALDATE \"18-Jul-1996 03:00:00 +0200\" RFC822.SIZE 64 ",
            "ENVELOPE (NIL \"Re: Ciao\" NIL NIL NIL NIL NIL NIL NIL NIL))"
        ))
        .await;
        conn.send_line("A0005 OK done").await;

        conn.expect("A0006 UID FETCH 2 (BODY.PEEK[])").await;
        conn.send("* 2 FETCH (UID 2 BODY[] {11}\r\n").await;
        conn.send("hello world)\r\n").await;
        conn.send_line("A0006 OK done").await;

        conn.expect("A0007 UID STORE 2 +FLAGS.SILENT (\\Seen)").await;
        conn.send_line("A0007 OK done").await;

        conn.expect("A0008 NOOP").await;
        conn.send_line("A0008 OK done").await;

        conn.expect("A0009 LOGOUT").await;
        conn.send_line("* BYE closing").await;
        conn.send_line("A0009 OK done").await;
    })
    .await;

    let mut session = Session::connect(server.config(&tls)).await.expect("connect");
    assert!(session.capabilities().imap4rev1());
    assert!(session.capabilities().auth_plain());
    assert!(session.capabilities().idle());

    session.login().await.expect("login");
    assert!(session.capabilities().idle(), "the login OK carried CAPABILITY");

    let folders = session.list().await.expect("list");
    assert_eq!(folders.len(), 3);
    assert_eq!(folders.first().map(|folder| folder.name.as_str()), Some("INBOX"));
    assert!(folders.first().is_some_and(|folder| folder.special_use == Some(postino_net::SpecialUse::Inbox)));
    assert!(folders.get(2).is_some_and(|folder| !folder.is_selectable()));

    let status = session.select("INBOX").await.expect("select");
    assert_eq!(status.exists, 2);
    assert_eq!(status.recent, 0);
    assert_eq!(status.uid_validity, 42);
    assert_eq!(status.uid_next, Some(10));
    assert_eq!(status.unseen, Some(1));
    assert!(!status.read_only);
    assert!(status.permanent_flags.contains(&Flag::Seen));

    let envelopes = session.fetch_envelopes(UidSet::parse("1:2").expect("uid set")).await.expect("fetch");
    assert_eq!(envelopes.len(), 2);
    let first = envelopes.first().expect("first envelope");
    assert_eq!(first.uid, 1);
    assert_eq!(first.subject, "Ciao");
    assert_eq!(first.size, 128);
    assert_eq!(first.flags, vec![Flag::Seen]);
    assert_eq!(first.from.first().and_then(|address| address.mailbox.as_deref()), Some("giulia"));
    assert_eq!(first.message_id.as_deref(), Some("<a@example.com>"));
    assert!(first.internal_date.is_some());
    assert!(first.references.is_empty() && first.list_id.is_none());

    let body = session.fetch_body(2).await.expect("body");
    assert_eq!(body, b"hello world");

    session.store_flags(UidSet::single(2).expect("uid"), &[Flag::Seen], FlagMode::Add).await.expect("store");
    session.noop().await.expect("noop");
    session.logout().await.expect("logout");
    server.finish().await;
}

#[tokio::test]
async fn list_decodes_modified_utf7_and_keeps_the_wire_name() {
    let tls = TlsFixture::generate();
    let server = FakeServer::start_tls(&tls, |mut conn| async move {
        conn.prepare("").await;
        conn.login_ok().await;
        conn.expect("A0003 LIST \"\" \"*\"").await;
        conn.send_line("* LIST (\\HasNoChildren) \"/\" \"~peter/mail/&U,BTFw-/&ZeVnLIqe-\"").await;
        conn.send_line("* LIST (\\Noselect) NIL \"Archivio\"").await;
        conn.send_line("A0003 OK done").await;
    })
    .await;

    let mut session = Session::connect(server.config(&tls)).await.expect("connect");
    session.login().await.expect("login");
    let folders = session.list().await.expect("list");
    assert_eq!(folders.len(), 2);
    let first = folders.first().expect("folder");
    assert_eq!(first.name, "~peter/mail/台北/日本語");
    assert_eq!(first.wire_name, "~peter/mail/&U,BTFw-/&ZeVnLIqe-");
    assert_eq!(decode_utf7(&first.wire_name).as_deref(), Some(first.name.as_str()));
    assert_eq!(folders.get(1).and_then(|folder| folder.delimiter), None);
    session.logout().await.expect("logout");
    server.finish().await;
}

#[tokio::test]
async fn login_line_quotes_the_credentials() {
    let tls = TlsFixture::generate();
    let server = FakeServer::start_tls(&tls, |mut conn| async move {
        conn.prepare("").await;
        conn.expect("A0002 LOGIN \"user@example.com\" \"correct horse battery\"").await;
        conn.send_line("A0002 OK logged in").await;
    })
    .await;
    let mut session = Session::connect(server.config(&tls)).await.expect("connect");
    session.login().await.expect("login");
    session.logout().await.expect("logout");
    server.finish().await;
    assert_eq!(USER, "user@example.com");
    assert_eq!(PASSWORD, "correct horse battery");
}
