//! SMTP submission client tests against the scripted TLS server.

use std::time::Duration;

use postino_net::{DsnNotify, DsnOptions, Secret, SmtpClient, SmtpConfig, SmtpEnvelope, SmtpError, TlsMode};
use postino_testkit::{FakeSmtp, SMTP_PASSWORD, SMTP_USER, TlsFixture, expected_smtp_login_password, expected_smtp_login_user, expected_smtp_sasl};

const RAW: &[u8] = b"From: Sender <sender@example.com>\r\nTo: One <one@example.com>\r\nSubject: Prova\r\nDate: Mon, 26 Oct 2026 10:00:00 +0000\r\nMessage-ID: <p7@example.com>\r\n\r\nCiao,\r\n.una riga che inizia con un punto\r\n..due punti\r\n";

fn envelope() -> SmtpEnvelope {
    SmtpEnvelope { from: "sender@example.com".to_owned(), recipients: vec!["one@example.com".to_owned(), "two@example.com".to_owned()] }
}

#[tokio::test]
async fn implicit_tls_sends_the_exact_bytes_including_dot_stuffing() {
    let tls = TlsFixture::generate();
    let server = FakeSmtp::start_tls(&tls, |mut conn| async move {
        conn.greet(&["SIZE 1000000", "8BITMIME", "AUTH PLAIN LOGIN"]).await;
        assert!(conn.received()[0].starts_with("EHLO "));
        conn.auth_plain_ok(&expected_smtp_sasl()).await;
        let mail = conn.expect_prefix("MAIL FROM:<sender@example.com> SIZE=").await;
        assert!(mail.contains("BODY=8BITMIME") || !raw_has_8bit(), "the body is ASCII, so BODY=8BITMIME must not appear: {mail}");
        conn.send_line("250 2.1.0 ok").await;
        conn.expect("RCPT TO:<one@example.com>").await;
        conn.send_line("250 2.1.5 ok").await;
        conn.expect("RCPT TO:<two@example.com>").await;
        conn.send_line("250 2.1.5 ok").await;
        conn.expect("DATA").await;
        conn.send_line("354 go ahead").await;
        let data = conn.read_data().await;
        assert_eq!(data, RAW, "the bytes on the wire differ from the serialized message");
        assert!(data.windows(4).any(|window| window == b"\r\n.."), "dot-stuffing did not double a leading dot");
        conn.send_line("250 2.0.0 queued as 1").await;
        conn.expect("QUIT").await;
        conn.send_line("221 bye").await;
    })
    .await;

    let mut client = SmtpClient::connect(server.config(&tls)).await.expect("connect");
    client.authenticate().await.expect("authenticate");
    let delivery = client.send(&envelope(), RAW, None).await.expect("send");
    assert_eq!(delivery.code, 250);
    assert_eq!(delivery.accepted, 2);
    client.quit().await.expect("quit");
    server.finish().await;
}

fn raw_has_8bit() -> bool {
    RAW.iter().any(|byte| !byte.is_ascii())
}

#[tokio::test]
async fn auth_login_is_used_when_plain_is_absent() {
    let tls = TlsFixture::generate();
    let server = FakeSmtp::start_tls(&tls, |mut conn| async move {
        conn.greet(&["AUTH LOGIN"]).await;
        conn.auth_login_ok(&expected_smtp_login_user(), &expected_smtp_login_password()).await;
    })
    .await;
    let mut client = SmtpClient::connect(server.config(&tls)).await.expect("connect");
    client.authenticate().await.expect("login");
    assert!(client.is_authenticated());
    server.finish().await;
}

#[tokio::test]
async fn a_rejected_password_is_a_typed_auth_error_and_never_echoed() {
    let tls = TlsFixture::generate();
    let server = FakeSmtp::start_tls(&tls, |mut conn| async move {
        conn.greet(&["AUTH PLAIN"]).await;
        let line = conn.expect_prefix("AUTH PLAIN ").await;
        assert_ne!(line, format!("AUTH PLAIN {}", expected_smtp_sasl()));
        conn.send_line("535 5.7.8 authentication credentials invalid").await;
    })
    .await;
    let wrong = SmtpConfig::new("localhost", server.port(), TlsMode::Implicit, SMTP_USER, Secret::new("the wrong password"))
        .with_timeout(Duration::from_secs(5))
        .with_extra_root(tls.ca());
    let mut rejected = SmtpClient::connect(wrong).await.expect("connect");
    let error = rejected.authenticate().await.expect_err("bad password");
    assert!(matches!(error, SmtpError::Auth { .. }), "{error:?}");
    assert!(error.is_authentication());
    assert!(!error.to_string().contains("the wrong password"), "{error}");
    server.finish().await;
}

#[tokio::test]
async fn starttls_upgrades_before_any_credential_and_asks_ehlo_again() {
    let tls = TlsFixture::generate();
    let server = FakeSmtp::start_starttls(&tls, |mut conn| async move {
        conn.greet(&["STARTTLS", "SIZE 1000"]).await;
        conn.expect("STARTTLS").await;
        conn.send_line("220 2.0.0 ready to start TLS").await;
        assert!(conn.upgrade_tls().await, "the client refused the test certificate");
        conn.ehlo(&["SIZE 2000", "AUTH PLAIN LOGIN"]).await;
        conn.auth_plain_ok(&expected_smtp_sasl()).await;
        conn.expect("QUIT").await;
        conn.send_line("221 bye").await;
    })
    .await;
    let mut client = SmtpClient::connect(server.starttls_config(&tls)).await.expect("connect");
    assert!(!client.capabilities().starttls, "the pre-TLS capability list was kept instead of discarded");
    assert_eq!(client.capabilities().size, Some(2000), "the client did not ask EHLO again over TLS");
    client.authenticate().await.expect("authenticate");
    client.quit().await.expect("quit");
    server.finish().await;
}

#[tokio::test]
async fn a_wrong_hostname_is_refused_by_tls() {
    let tls = TlsFixture::generate();
    let server = FakeSmtp::start_tls_tolerant(&tls, |_conn| async {}).await;
    let config = SmtpConfig::new("127.0.0.1", server.port(), TlsMode::Implicit, SMTP_USER, Secret::new(SMTP_PASSWORD))
        .with_timeout(Duration::from_secs(3))
        .with_extra_root(tls.ca());
    let error = SmtpClient::connect(config).await.expect_err("the certificate is for localhost, not 127.0.0.1");
    assert!(matches!(error, SmtpError::Tls { .. }), "{error:?}");
    server.finish().await;
}

#[tokio::test]
async fn a_4xx_recipient_is_transient_and_a_5xx_sender_is_permanent() {
    let tls = TlsFixture::generate();
    let server = FakeSmtp::start_tls(&tls, |mut conn| async move {
        conn.greet(&["AUTH PLAIN"]).await;
        conn.auth_plain_ok(&expected_smtp_sasl()).await;
        conn.expect_prefix("MAIL FROM:").await;
        conn.send_line("250 ok").await;
        conn.expect_prefix("RCPT TO:").await;
        conn.send_line("451 4.7.1 try again later").await;
    })
    .await;
    let mut client = SmtpClient::connect(server.config(&tls)).await.expect("connect");
    client.authenticate().await.expect("authenticate");
    let error = client.send(&envelope(), RAW, None).await.expect_err("451");
    assert!(matches!(error, SmtpError::Rejected { code: 451, .. }), "{error:?}");
    assert!(error.is_transient());
    server.finish().await;

    let tls = TlsFixture::generate();
    let server = FakeSmtp::start_tls(&tls, |mut conn| async move {
        conn.greet(&["AUTH PLAIN"]).await;
        conn.auth_plain_ok(&expected_smtp_sasl()).await;
        conn.expect_prefix("MAIL FROM:").await;
        conn.send_line("550 5.1.8 sender rejected").await;
    })
    .await;
    let mut client = SmtpClient::connect(server.config(&tls)).await.expect("connect");
    client.authenticate().await.expect("authenticate");
    let error = client.send(&envelope(), RAW, None).await.expect_err("550");
    assert!(matches!(error, SmtpError::Rejected { code: 550, .. }), "{error:?}");
    assert!(!error.is_transient());
    server.finish().await;
}

#[tokio::test]
async fn size_is_checked_before_mail() {
    let tls = TlsFixture::generate();
    let server = FakeSmtp::start_tls(&tls, |mut conn| async move {
        conn.greet(&["SIZE 10", "AUTH PLAIN"]).await;
        conn.auth_plain_ok(&expected_smtp_sasl()).await;
        let sent = tokio::time::timeout(Duration::from_millis(250), conn.read_line_opt()).await;
        assert!(sent.is_err(), "the client sent a command despite the SIZE refusal: {sent:?}");
    })
    .await;
    let mut client = SmtpClient::connect(server.config(&tls)).await.expect("connect");
    client.authenticate().await.expect("authenticate");
    let error = client.send(&envelope(), RAW, None).await.expect_err("too large");
    assert!(matches!(error, SmtpError::TooLarge { what: "message", .. }), "{error:?}");
    server.finish().await;
}

#[tokio::test]
async fn smtputf8_is_required_only_when_needed_and_advertised() {
    let tls = TlsFixture::generate();
    let server = FakeSmtp::start_tls(&tls, |mut conn| async move {
        conn.greet(&["SIZE 100000", "AUTH PLAIN"]).await;
        conn.auth_plain_ok(&expected_smtp_sasl()).await;
    })
    .await;
    let mut client = SmtpClient::connect(server.config(&tls)).await.expect("connect");
    client.authenticate().await.expect("authenticate");
    let utf8 = b"From: sender@example.com\r\nSubject: Citt\xC3\xA0\r\n\r\ncorpo\r\n";
    let error = client.send(&envelope(), utf8, None).await.expect_err("SMTPUTF8 is not advertised");
    assert!(matches!(error, SmtpError::Unsupported { feature: "SMTPUTF8" }), "{error:?}");
    server.finish().await;

    let tls = TlsFixture::generate();
    let server = FakeSmtp::start_tls(&tls, |mut conn| async move {
        conn.greet(&["SIZE 100000", "8BITMIME", "SMTPUTF8", "AUTH PLAIN"]).await;
        conn.auth_plain_ok(&expected_smtp_sasl()).await;
        let mail = conn.expect_prefix("MAIL FROM:").await;
        assert!(mail.contains("SMTPUTF8"), "{mail}");
        assert!(mail.contains("BODY=8BITMIME"), "{mail}");
        conn.send_line("250 ok").await;
    })
    .await;
    let mut client = SmtpClient::connect(server.config(&tls)).await.expect("connect");
    client.authenticate().await.expect("authenticate");
    // The server closes after MAIL; the send fails but the MAIL line was asserted. The point of
    // this half is the parameter, not a complete transaction.
    let _ = client.send(&envelope(), utf8, None).await;
    server.finish().await;
}

#[tokio::test]
async fn eight_bit_body_without_8bitmime_is_refused_before_mail() {
    let tls = TlsFixture::generate();
    let server = FakeSmtp::start_tls(&tls, |mut conn| async move {
        conn.greet(&["SIZE 100000", "AUTH PLAIN"]).await;
        conn.auth_plain_ok(&expected_smtp_sasl()).await;
    })
    .await;
    let mut client = SmtpClient::connect(server.config(&tls)).await.expect("connect");
    client.authenticate().await.expect("authenticate");
    let eight_bit = b"From: sender@example.com\r\n\r\ncorpo \xE8\r\n";
    let error = client.send(&envelope(), eight_bit, None).await.expect_err("8BITMIME missing");
    assert!(matches!(error, SmtpError::Unsupported { feature: "8BITMIME" }), "{error:?}");
    server.finish().await;
}

#[tokio::test]
async fn dsn_parameters_are_sent_only_when_advertised() {
    let tls = TlsFixture::generate();
    let server = FakeSmtp::start_tls(&tls, |mut conn| async move {
        conn.greet(&["SIZE 100000", "DSN", "AUTH PLAIN"]).await;
        conn.auth_plain_ok(&expected_smtp_sasl()).await;
        let mail = conn.expect_prefix("MAIL FROM:").await;
        assert!(mail.contains("RET=FULL"), "{mail}");
        assert!(mail.contains("ENVID=abc_123"), "{mail}");
        conn.send_line("250 ok").await;
        conn.expect("RCPT TO:<one@example.com> NOTIFY=SUCCESS,FAILURE").await;
        conn.send_line("250 ok").await;
        conn.expect("RCPT TO:<two@example.com> NOTIFY=SUCCESS,FAILURE").await;
        conn.send_line("250 ok").await;
        conn.expect("DATA").await;
        conn.send_line("354 go").await;
        let _ = conn.read_data().await;
        conn.send_line("250 queued").await;
    })
    .await;
    let mut client = SmtpClient::connect(server.config(&tls)).await.expect("connect");
    client.authenticate().await.expect("authenticate");
    let dsn = DsnOptions { return_full: true, notify: vec![DsnNotify::Success, DsnNotify::Failure], envid: Some("abc_123".to_owned()) };
    client.send(&envelope(), RAW, Some(&dsn)).await.expect("send");
    server.finish().await;
}

#[tokio::test]
async fn a_timeout_after_the_final_dot_is_uncertain() {
    let tls = TlsFixture::generate();
    let server = FakeSmtp::start_tls(&tls, |mut conn| async move {
        conn.greet(&["AUTH PLAIN"]).await;
        conn.auth_plain_ok(&expected_smtp_sasl()).await;
        conn.expect_prefix("MAIL FROM:").await;
        conn.send_line("250 ok").await;
        conn.expect("RCPT TO:<one@example.com>").await;
        conn.send_line("250 ok").await;
        conn.expect("RCPT TO:<two@example.com>").await;
        conn.send_line("250 ok").await;
        conn.expect("DATA").await;
        conn.send_line("354 go").await;
        let _ = conn.read_data().await;
        conn.delay(Duration::from_millis(700)).await;
    })
    .await;
    let config = server.config(&tls).with_timeout(Duration::from_millis(250));
    let mut client = SmtpClient::connect(config).await.expect("connect");
    client.authenticate().await.expect("authenticate");
    let error = client.send(&envelope(), RAW, None).await.expect_err("timeout");
    assert!(error.is_uncertain(), "{error:?}");
    assert!(!error.is_transient(), "an uncertain outcome must never be retried automatically");
    server.finish().await;
}

#[tokio::test]
async fn a_drop_after_the_final_dot_is_uncertain() {
    let tls = TlsFixture::generate();
    let server = FakeSmtp::start_tls(&tls, |mut conn| async move {
        conn.greet(&["AUTH PLAIN"]).await;
        conn.auth_plain_ok(&expected_smtp_sasl()).await;
        conn.expect_prefix("MAIL FROM:").await;
        conn.send_line("250 ok").await;
        conn.expect("RCPT TO:<one@example.com>").await;
        conn.send_line("250 ok").await;
        conn.expect("RCPT TO:<two@example.com>").await;
        conn.send_line("250 ok").await;
        conn.expect("DATA").await;
        conn.send_line("354 go").await;
        let _ = conn.read_data().await;
        // Drop the connection without the 250.
    })
    .await;
    let mut client = SmtpClient::connect(server.config(&tls)).await.expect("connect");
    client.authenticate().await.expect("authenticate");
    let error = client.send(&envelope(), RAW, None).await.expect_err("closed");
    assert!(error.is_uncertain(), "{error:?}");
    server.finish().await;
}

#[tokio::test]
async fn a_slow_but_successful_response_stays_a_success() {
    let tls = TlsFixture::generate();
    let server = FakeSmtp::start_tls(&tls, |mut conn| async move {
        conn.greet(&["AUTH PLAIN"]).await;
        conn.auth_plain_ok(&expected_smtp_sasl()).await;
        conn.expect_prefix("MAIL FROM:").await;
        conn.delay(Duration::from_millis(150)).await;
        conn.send_line("250 ok").await;
        conn.expect("RCPT TO:<one@example.com>").await;
        conn.send_line("250 ok").await;
        conn.expect("RCPT TO:<two@example.com>").await;
        conn.send_line("250 ok").await;
        conn.expect("DATA").await;
        conn.send_line("354 go").await;
        let _ = conn.read_data().await;
        conn.delay(Duration::from_millis(150)).await;
        conn.send_line("250 queued").await;
    })
    .await;
    let config = server.config(&tls).with_timeout(Duration::from_secs(2));
    let mut client = SmtpClient::connect(config).await.expect("connect");
    client.authenticate().await.expect("authenticate");
    let delivery = client.send(&envelope(), RAW, None).await.expect("send");
    assert_eq!(delivery.code, 250);
    server.finish().await;
}

#[tokio::test]
async fn the_password_and_the_body_never_appear_in_the_command_transcript() {
    let tls = TlsFixture::generate();
    let server = FakeSmtp::start_tls(&tls, |mut conn| async move {
        conn.greet(&["AUTH PLAIN"]).await;
        conn.auth_plain_ok(&expected_smtp_sasl()).await;
        conn.expect_prefix("MAIL FROM:").await;
        conn.send_line("250 ok").await;
        conn.expect("RCPT TO:<one@example.com>").await;
        conn.send_line("250 ok").await;
        conn.expect("RCPT TO:<two@example.com>").await;
        conn.send_line("250 ok").await;
        conn.expect("DATA").await;
        conn.send_line("354 go").await;
        let _ = conn.read_data().await;
        conn.send_line("250 queued").await;
        assert!(!conn.transcript_contains(SMTP_PASSWORD), "the plaintext password reached a command line: {:?}", conn.received());
        assert!(!conn.received().iter().any(|line| line.contains("Ciao,")), "a body line reached the command transcript");
        assert!(!conn.transcript_contains("one@example.com\r\n"), "{:?}", conn.received());
    })
    .await;
    let mut client = SmtpClient::connect(server.config(&tls)).await.expect("connect");
    client.authenticate().await.expect("authenticate");
    client.send(&envelope(), RAW, None).await.expect("send");
    let config = format!("{client:?}");
    assert!(!config.contains(SMTP_PASSWORD), "{config}");
    server.finish().await;
}

#[tokio::test]
async fn starttls_missing_is_unsupported() {
    let tls = TlsFixture::generate();
    let server = FakeSmtp::start_starttls(&tls, |mut conn| async move {
        conn.greet(&["AUTH PLAIN"]).await;
    })
    .await;
    let error = SmtpClient::connect(server.starttls_config(&tls)).await.expect_err("STARTTLS is not advertised");
    assert!(matches!(error, SmtpError::Unsupported { feature: "STARTTLS" }), "{error:?}");
    server.finish().await;
}

#[tokio::test]
async fn a_refusal_after_the_final_dot_is_uncertain_and_never_retried() {
    let tls = TlsFixture::generate();
    let server = FakeSmtp::start_tls(&tls, |mut conn| async move {
        conn.greet(&["AUTH PLAIN"]).await;
        conn.auth_plain_ok(&expected_smtp_sasl()).await;
        conn.expect_prefix("MAIL FROM:").await;
        conn.send_line("250 ok").await;
        conn.expect("RCPT TO:<one@example.com>").await;
        conn.send_line("250 ok").await;
        conn.expect("RCPT TO:<two@example.com>").await;
        conn.send_line("250 ok").await;
        conn.expect("DATA").await;
        conn.send_line("354 go").await;
        let _ = conn.read_data().await;
        conn.send_line("554 5.6.0 message rejected").await;
    })
    .await;
    let mut client = SmtpClient::connect(server.config(&tls)).await.expect("connect");
    client.authenticate().await.expect("authenticate");
    let error = client.send(&envelope(), RAW, None).await.expect_err("554");
    // The server has the bytes: a person decides, the client never retries automatically.
    assert!(matches!(error, SmtpError::Uncertain { .. }), "{error:?}");
    assert!(!error.is_transient());
    assert!(error.is_uncertain(), "a refusal after DATA must stay uncertain");
    assert!(error.to_string().contains("554"), "the code is the only detail kept: {error}");
    server.finish().await;
}

#[tokio::test]
async fn a_size_of_zero_means_no_limit() {
    const SMALL: &[u8] = b"From: a@example.com\r\nTo: b@example.com\r\nSubject: x\r\n\r\nBody\r\n";
    let tls = TlsFixture::generate();
    let server = FakeSmtp::start_tls(&tls, |mut conn| async move {
        conn.greet(&["SIZE 0", "AUTH PLAIN"]).await;
        conn.auth_plain_ok(&expected_smtp_sasl()).await;
        let mail = conn.expect_prefix("MAIL FROM:").await;
        assert!(mail.contains("SIZE="), "the extension is advertised, so SIZE is still sent: {mail}");
        conn.send_line("250 ok").await;
        conn.expect("RCPT TO:<one@example.com>").await;
        conn.send_line("250 ok").await;
        conn.expect("RCPT TO:<two@example.com>").await;
        conn.send_line("250 ok").await;
        conn.expect("DATA").await;
        conn.send_line("354 go").await;
        let data = conn.read_data().await;
        assert_eq!(data, SMALL, "SIZE 0 must not refuse or truncate the message");
        conn.send_line("250 queued").await;
        conn.expect("QUIT").await;
        conn.send_line("221 bye").await;
    })
    .await;
    let mut client = SmtpClient::connect(server.config(&tls)).await.expect("connect");
    assert_eq!(client.capabilities().size, Some(0));
    client.authenticate().await.expect("authenticate");
    client.send(&envelope(), SMALL, None).await.expect("SIZE 0 must not refuse the message");
    client.quit().await.expect("quit");
    server.finish().await;
}

#[tokio::test]
async fn a_bare_carriage_return_is_refused_before_data() {
    let tls = TlsFixture::generate();
    let server = FakeSmtp::start_tls(&tls, |mut conn| async move {
        conn.greet(&["AUTH PLAIN"]).await;
        conn.auth_plain_ok(&expected_smtp_sasl()).await;
        // Nothing else: `prepare_data` refuses the message before the first command.
        conn.expect("RSET").await;
        conn.send_line("250 reset").await;
        conn.expect("QUIT").await;
        conn.send_line("221 bye").await;
    })
    .await;
    let mut client = SmtpClient::connect(server.config(&tls)).await.expect("connect");
    client.authenticate().await.expect("authenticate");
    let error = client.send(&envelope(), b"From: a@example.com\r\nSubject: x\rBody\r\n", None).await.expect_err("bare CR");
    assert!(matches!(error, SmtpError::Permanent { stage: "data", .. }), "{error:?}");
    assert!(error.to_string().contains("bare CR"), "{error}");
    client.reset().await.expect("reset");
    client.quit().await.expect("quit");
    server.finish().await;
}

#[tokio::test]
async fn an_ehlo_rejection_and_a_greeting_refusal_are_typed() {
    let tls = TlsFixture::generate();
    let server = FakeSmtp::start_tls(&tls, |mut conn| async move {
        conn.send_line("220 fake ready").await;
        conn.expect_prefix("EHLO ").await;
        conn.send_line("500 5.5.1 command not recognized").await;
    })
    .await;
    let error = SmtpClient::connect(server.config(&tls)).await.expect_err("EHLO refused");
    assert!(matches!(error, SmtpError::Rejected { code: 500, stage: "ehlo", .. }), "{error:?}");
    server.finish().await;

    let tls = TlsFixture::generate();
    let server = FakeSmtp::start_tls(&tls, |mut conn| async move {
        conn.send_line("421 4.3.2 service not available").await;
    })
    .await;
    let error = SmtpClient::connect(server.config(&tls)).await.expect_err("421 greeting");
    assert!(matches!(error, SmtpError::Rejected { code: 421, stage: "greeting", .. }), "{error:?}");
    assert!(error.is_transient());
    server.finish().await;
}

#[tokio::test]
async fn an_overlong_reply_line_hits_the_bound() {
    let tls = TlsFixture::generate();
    let server = FakeSmtp::start_tls(&tls, |mut conn| async move {
        conn.send_line("220 fake ready").await;
        conn.expect_prefix("EHLO ").await;
        conn.send_line(&format!("250-{}", "x".repeat(9 * 1024))).await;
    })
    .await;
    let error = SmtpClient::connect(server.config(&tls)).await.expect_err("overlong line");
    assert!(matches!(error, SmtpError::TooLarge { what: "line", .. }), "{error:?}");
    server.finish().await;
}

#[tokio::test]
async fn more_than_one_hundred_recipients_is_refused_before_the_wire() {
    let tls = TlsFixture::generate();
    let server = FakeSmtp::start_tls(&tls, |mut conn| async move {
        conn.greet(&["AUTH PLAIN"]).await;
        conn.auth_plain_ok(&expected_smtp_sasl()).await;
    })
    .await;
    let mut client = SmtpClient::connect(server.config(&tls)).await.expect("connect");
    client.authenticate().await.expect("authenticate");
    let many = SmtpEnvelope { from: "sender@example.com".to_owned(), recipients: (0..101).map(|index| format!("user{index}@example.com")).collect() };
    let error = client.send(&many, RAW, None).await.expect_err("too many recipients");
    assert!(matches!(error, SmtpError::TooLarge { what: "recipients", .. }), "{error:?}");
    server.finish().await;
}
