//! A deterministic corpus of malformed server responses.
//!
//! Each case is a real scripted conversation: the server sends the payload at a specific phase and
//! the client must return the expected class of [`NetError`] quickly. A case that panics, hangs,
//! returns the wrong class or is accepted fails the test.

mod support;

use std::time::Duration;

use postino_net::{NetError, Session, UidSet};
use support::{FakeServer, TlsFixture};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Greeting,
    Capability,
    Login,
    Select,
    Fetch,
}

/// The error class a case must produce. Asserting the class, not merely "some error", catches a
/// parser that silently reclassifies a malformed response as a timeout or a disconnect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Class {
    Protocol,
    TooLarge,
    Closed,
    Timeout,
    Auth,
    ServerNo,
    Unsupported,
}

struct Case {
    name: &'static str,
    phase: Phase,
    payload: Vec<u8>,
    close: bool,
    expect: Class,
}

impl Case {
    fn new(name: &'static str, phase: Phase, payload: impl Into<Vec<u8>>) -> Self {
        Self { name, phase, payload: payload.into(), close: false, expect: Class::Protocol }
    }

    fn expect(mut self, class: Class) -> Self {
        self.expect = class;
        self
    }

    fn closing(mut self) -> Self {
        self.close = true;
        self
    }
}

async fn run(case: &Case, tls: &TlsFixture) -> Result<(), NetError> {
    let phase = case.phase;
    let payload = case.payload.clone();
    let close = case.close;
    let server = FakeServer::start_tls(tls, move |mut conn| async move {
        if phase == Phase::Greeting {
            conn.send_bytes(&payload).await;
            if !close {
                conn.delay(Duration::from_secs(2)).await;
            }
            return;
        }
        conn.send_line("* OK fake IMAP ready").await;
        conn.expect("A0001 CAPABILITY").await;
        if phase == Phase::Capability {
            conn.send_bytes(&payload).await;
        } else {
            conn.send_line("* CAPABILITY IMAP4rev1").await;
            conn.send_line("A0001 OK done").await;
            match phase {
                Phase::Login => {
                    conn.read_line().await;
                    conn.send_bytes(&payload).await;
                }
                Phase::Select => {
                    conn.read_line().await;
                    conn.send_line("A0002 OK logged in").await;
                    conn.read_line().await;
                    conn.send_bytes(&payload).await;
                }
                Phase::Fetch => {
                    conn.read_line().await;
                    conn.send_line("A0002 OK logged in").await;
                    conn.read_line().await;
                    conn.send_line("* 0 EXISTS").await;
                    conn.send_line("* OK [UIDVALIDITY 1] ok").await;
                    conn.send_line("A0003 OK selected").await;
                    conn.read_line().await;
                    conn.send_bytes(&payload).await;
                }
                Phase::Greeting | Phase::Capability => {}
            }
        }
        if !close {
            conn.delay(Duration::from_secs(2)).await;
        }
    })
    .await;

    let config = server.config(tls).with_timeout(Duration::from_millis(400));
    let result = async {
        let mut session = Session::connect(config).await?;
        match phase {
            Phase::Greeting | Phase::Capability => Ok(()),
            Phase::Login => session.login().await,
            Phase::Select => {
                session.login().await?;
                session.select("INBOX").await.map(|_| ())
            }
            Phase::Fetch => {
                session.login().await?;
                session.select("INBOX").await?;
                session.fetch_envelopes(UidSet::single(1).expect("uid")).await.map(|_| ())
            }
        }
    }
    .await;
    drop(server);
    result
}

fn class_of(error: &NetError) -> Class {
    match error {
        NetError::Protocol { .. } => Class::Protocol,
        NetError::TooLarge { .. } => Class::TooLarge,
        NetError::Closed { .. } => Class::Closed,
        NetError::Timeout { .. } => Class::Timeout,
        NetError::Auth { .. } => Class::Auth,
        NetError::ServerNo { .. } => Class::ServerNo,
        NetError::Unsupported { .. } => Class::Unsupported,
        NetError::ServerBad { .. } | NetError::Dns { .. } | NetError::Connect { .. } | NetError::Tls { .. } => {
            panic!("the corpus never provokes {error:?}")
        }
    }
}

fn corpus() -> Vec<Case> {
    let deep = format!("* 1 FETCH (UID 1 X {}{})\r\nA0001 OK done\r\n", "(".repeat(40), ")".repeat(40));
    let cases = vec![
        // Greeting.
        Case::new("greeting garbage", Phase::Greeting, "* hello there\r\n"),
        Case::new("greeting bye", Phase::Greeting, "* BYE maintenance\r\n").expect(Class::Closed),
        Case::new("greeting tagged", Phase::Greeting, "A0001 OK hello\r\n"),
        Case::new("greeting silent", Phase::Greeting, "").expect(Class::Timeout),
        Case::new("greeting bare lf", Phase::Greeting, "* OK hi\n\r\n"),
        Case::new("greeting bad status", Phase::Greeting, "* MAYBE ready\r\n"),
        // Capability.
        Case::new("capability unknown status", Phase::Capability, "* CAPABILITY IMAP4rev1\r\nA0001 MAYBE done\r\n"),
        Case::new("capability wrong tag", Phase::Capability, "* CAPABILITY IMAP4rev1\r\nB0001 OK done\r\n"),
        Case::new("capability missing rev1", Phase::Capability, "* CAPABILITY UIDPLUS\r\nA0001 OK done\r\n").expect(Class::Unsupported),
        Case::new("capability control character", Phase::Capability, "* CAPABILITY IMAP4rev1 \u{1}X\r\nA0001 OK done\r\n"),
        Case::new("capability literal where an atom belongs", Phase::Capability, "* CAPABILITY IMAP4rev1 {5}\r\nhello\r\nA0001 OK done\r\n"),
        Case::new("capability huge literal marker", Phase::Capability, "* CAPABILITY {999999999999999999999}\r\n"),
        Case::new("capability tagged without status", Phase::Capability, "A0001\r\n"),
        Case::new("capability missing asterisk", Phase::Capability, "OK done\r\n"),
        Case::new("capability exists junk", Phase::Capability, "* 3 EXISTS junk\r\nA0001 OK done\r\n"),
        Case::new("capability negative number", Phase::Capability, "* -1 EXISTS\r\nA0001 OK done\r\n"),
        Case::new("capability nil where a number belongs", Phase::Capability, "* NIL EXISTS\r\nA0001 OK done\r\n"),
        Case::new("capability huge number", Phase::Capability, "* 99999999999999999999 EXISTS\r\nA0001 OK done\r\n"),
        Case::new("capability fetch nil uid", Phase::Capability, "* 1 FETCH (UID NIL)\r\nA0001 OK done\r\n"),
        Case::new("capability fetch negative size", Phase::Capability, "* 1 FETCH (RFC822.SIZE -1)\r\nA0001 OK done\r\n"),
        Case::new("capability deep nesting", Phase::Capability, deep.clone()),
        Case::new("capability list nil name", Phase::Capability, "* LIST (\\Sent) \"/\" NIL\r\nA0001 OK done\r\n"),
        Case::new("capability list two-char delimiter", Phase::Capability, "* LIST (\\Sent) \"ab\" \"X\"\r\nA0001 OK done\r\n"),
        Case::new("capability list missing name", Phase::Capability, "* LIST (\\Sent) \"/\"\r\nA0001 OK done\r\n"),
        Case::new("capability list unterminated attributes", Phase::Capability, "* LIST (\\Sent \"/\" \"X\"\r\nA0001 OK done\r\n"),
        Case::new("capability unterminated quoted", Phase::Capability, "* LIST (\\Sent) \"/\" \"X\r\nA0001 OK done\r\n"),
        Case::new("capability line too long", Phase::Capability, format!("* CAPABILITY {}\r\n", "A".repeat(70_000))).expect(Class::TooLarge),
        // Login.
        Case::new("login no", Phase::Login, "A0002 NO [AUTHENTICATIONFAILED] nope\r\n").expect(Class::Auth),
        Case::new("login bad", Phase::Login, "A0002 BAD bad command\r\n").expect(Class::Auth),
        Case::new("login unexpected continuation", Phase::Login, "+ weird\r\n"),
        Case::new("login garbage", Phase::Login, "garbage\r\n"),
        // Select.
        Case::new("select missing uidvalidity", Phase::Select, "* 1 EXISTS\r\nA0003 OK done\r\n"),
        Case::new("select uidvalidity nil", Phase::Select, "* 1 EXISTS\r\n* OK [UIDVALIDITY NIL] x\r\nA0003 OK done\r\n"),
        Case::new("select uidvalidity zero", Phase::Select, "* 1 EXISTS\r\n* OK [UIDVALIDITY 0] x\r\nA0003 OK done\r\n"),
        Case::new("select uidvalidity overflow", Phase::Select, "* 1 EXISTS\r\n* OK [UIDVALIDITY 4294967296] x\r\nA0003 OK done\r\n"),
        Case::new("select negative exists", Phase::Select, "* -1 EXISTS\r\nA0003 OK done\r\n"),
        Case::new("select nil exists", Phase::Select, "* NIL EXISTS\r\nA0003 OK done\r\n"),
        Case::new("select unterminated code", Phase::Select, "* OK [UIDVALIDITY 4 x\r\nA0003 OK done\r\n"),
        Case::new("select unknown system flag", Phase::Select, "* OK [PERMANENTFLAGS (\\Bogus)] x\r\nA0003 OK done\r\n"),
        Case::new("select wrong tag", Phase::Select, "* 1 EXISTS\r\n* OK [UIDVALIDITY 1] x\r\nA9999 OK done\r\n"),
        Case::new("select no", Phase::Select, "A0003 NO [NONEXISTENT] gone\r\n").expect(Class::ServerNo),
        // Fetch.
        Case::new("fetch literal without terminator", Phase::Fetch, "* 1 FETCH (UID 1 BODY[] {50}\r\nshort").closing().expect(Class::Closed),
        Case::new("fetch unterminated list", Phase::Fetch, "* 1 FETCH (UID 1\r\nA0004 OK done\r\n"),
        Case::new("fetch envelope arity", Phase::Fetch, "* 1 FETCH (UID 1 ENVELOPE (\"a\" \"b\"))\r\nA0004 OK done\r\n"),
        Case::new("fetch negative size", Phase::Fetch, "* 1 FETCH (UID 1 RFC822.SIZE -1)\r\nA0004 OK done\r\n"),
        Case::new("fetch size overflow", Phase::Fetch, "* 1 FETCH (UID 1 RFC822.SIZE 99999999999)\r\nA0004 OK done\r\n"),
        Case::new("fetch bad internaldate", Phase::Fetch, "* 1 FETCH (UID 1 INTERNALDATE \"not a date\")\r\nA0004 OK done\r\n"),
        Case::new("fetch unterminated flags", Phase::Fetch, "* 1 FETCH (UID 1 FLAGS (\\Seen)\r\nA0004 OK done\r\n"),
        Case::new("fetch sequence zero", Phase::Fetch, "* 0 FETCH (UID 1)\r\nA0004 OK done\r\n"),
        Case::new("fetch trailing junk", Phase::Fetch, "* 1 FETCH (UID 1) junk\r\nA0004 OK done\r\n"),
        Case::new("fetch unclosed section", Phase::Fetch, "* 1 FETCH (UID 1 BODY[HEADER\r\nA0004 OK done\r\n"),
        Case::new("fetch nested unknown item", Phase::Fetch, deep.replace("A0001", "A0004")),
        Case::new(
            "fetch message id as list",
            Phase::Fetch,
            "* 1 FETCH (UID 1 ENVELOPE (NIL NIL NIL NIL NIL NIL NIL NIL NIL (\"x\")))\r\nA0004 OK done\r\n",
        ),
        Case::new("fetch bad flag", Phase::Fetch, "* 1 FETCH (UID 1 FLAGS (\\Bogus))\r\nA0004 OK done\r\n"),
    ];
    assert!(cases.len() >= 30);
    cases
}

#[tokio::test]
async fn malformed_responses_are_typed_errors_not_panics() {
    let tls = TlsFixture::generate();
    for case in corpus() {
        let outcome = tokio::time::timeout(Duration::from_secs(3), run(&case, &tls)).await;
        match outcome {
            Err(_) => panic!("{} hung", case.name),
            Ok(Ok(())) => panic!("{} was accepted by the client", case.name),
            Ok(Err(error)) => {
                let text = error.to_string();
                assert_eq!(class_of(&error), case.expect, "{} gave {error:?}", case.name);
                assert!(text.is_ascii(), "{} produced a non-ASCII error: {text}", case.name);
                assert!(text.len() < 400, "{} produced an unbounded error: {text}", case.name);
            }
        }
    }
}
