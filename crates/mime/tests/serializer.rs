use chrono::{DateTime, Utc};
use postino_mime::{Envelope, Limits, Mailbox, MimeError, OutgoingAttachment, OutgoingMessage, parse, sanitize_html, serialize};

fn date() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-10-25T10:42:00Z").expect("date").with_timezone(&Utc)
}

fn mailbox(name: &str, address: &str) -> Mailbox {
    Mailbox { name: (!name.is_empty()).then(|| name.to_owned()), address: address.to_owned() }
}

fn message() -> OutgoingMessage {
    OutgoingMessage {
        from: mailbox("Giulia Ferri", "giulia@example.com"),
        to: vec![mailbox("Marco Verdi", "marco@example.it")],
        cc: vec![mailbox("", "ufficio@example.it")],
        bcc: vec![mailbox("", "audit@example.com")],
        subject: "Città è pronta 👋".to_owned(),
        date: date(),
        text: "Ciao Marco,\n\nCittà è pronta.\n".to_owned(),
        html: Some("<p>Città è <strong>pronta</strong>.</p><ul><li>uno</li></ul>".to_owned()),
        attachments: vec![OutgoingAttachment {
            filename: "Relazione finale è pronta.pdf".to_owned(),
            mime: "application/pdf".to_owned(),
            data: b"%PDF-1.4\n%EOF\n".to_vec(),
            content_id: None,
        }],
        in_reply_to: Some("parent@example.com".to_owned()),
        references: vec!["root@example.com".to_owned(), "parent@example.com".to_owned()],
        message_id: None,
        extra_headers: Vec::new(),
    }
}

fn normalize_newlines(text: &str) -> String {
    text.replace("\r\n", "\n")
}

fn raw_text(serialized: &postino_mime::Serialized) -> String {
    String::from_utf8(serialized.raw.clone()).expect("serialized bytes are ASCII")
}

#[test]
fn round_trip_preserves_the_contract() {
    let message = message();
    let serialized = serialize(&message).expect("serialize");
    let parsed = parse(&serialized.raw, &Limits::default()).expect("parse");
    assert_eq!(parsed.subject, message.subject);
    assert_eq!(parsed.from[0].name.as_deref(), Some("Giulia Ferri"));
    assert_eq!(parsed.from[0].address, "giulia@example.com");
    assert_eq!(parsed.to[0].name.as_deref(), Some("Marco Verdi"));
    assert_eq!(parsed.to[0].address, "marco@example.it");
    assert_eq!(parsed.cc[0].address, "ufficio@example.it");
    assert!(parsed.bcc.is_empty());
    assert_eq!(normalize_newlines(parsed.text.as_deref().expect("text")), normalize_newlines(&message.text));
    let expected_html = sanitize_html(message.html.as_deref().expect("html"), Limits::default().max_html_bytes);
    assert_eq!(parsed.html.as_deref(), Some(expected_html.html.as_str()));
    assert_eq!(parsed.attachments.len(), 1);
    assert_eq!(parsed.attachments[0].filename, message.attachments[0].filename);
    assert_eq!(parsed.attachments[0].mime, "application/pdf");
    assert_eq!(parsed.attachments[0].data, message.attachments[0].data);
    assert_eq!(parsed.in_reply_to.as_deref(), Some("parent@example.com"));
    assert_eq!(parsed.references, ["root@example.com", "parent@example.com"]);
    assert_eq!(parsed.date, Some(message.date));
    assert_eq!(parsed.message_id.as_deref().map(str::len), Some(32 + 1 + 11), "generated id length");
    assert_eq!(serialized.envelope.from, "giulia@example.com");
}

#[test]
fn bcc_never_reaches_the_bytes() {
    let message = message();
    let serialized = serialize(&message).expect("serialize");
    let raw = raw_text(&serialized);
    assert!(!raw.contains("audit@example.com"), "{raw}");
    assert!(!raw.to_ascii_lowercase().contains("bcc:"), "{raw}");
    assert!(raw.contains("To: Marco Verdi <marco@example.it>"));
    assert!(raw.contains("Cc: ufficio@example.it"));
    assert!(serialized.envelope.recipients.contains(&"audit@example.com".to_owned()));
    assert_eq!(serialized.envelope.recipients, ["marco@example.it", "ufficio@example.it", "audit@example.com"]);
}

#[test]
fn identical_messages_serialize_identically() {
    let first = serialize(&message()).expect("first");
    let second = serialize(&message()).expect("second");
    assert_eq!(first.raw, second.raw);
    assert_eq!(first.envelope, second.envelope);
}

#[test]
fn injection_is_rejected_not_stripped() {
    let mut injected = message();
    injected.subject = "Ciao\r\nBcc: evil@example.com".to_owned();
    assert!(matches!(serialize(&injected), Err(MimeError::HeaderInjection { header: "Subject" })));

    let mut injected = message();
    injected.to[0].name = Some("Marco\r\nBcc: evil@example.com".to_owned());
    assert!(matches!(serialize(&injected), Err(MimeError::HeaderInjection { header: "To" })));

    let mut injected = message();
    injected.attachments[0].filename = "relazione\r\nBcc: evil@example.com.pdf".to_owned();
    assert!(matches!(serialize(&injected), Err(MimeError::HeaderInjection { header: "Content-Disposition" })));

    let mut injected = message();
    injected.subject = "nul\0byte".to_owned();
    assert!(matches!(serialize(&injected), Err(MimeError::HeaderInjection { .. })));

    let mut injected = message();
    injected.from.address = "giulia@example.com\r\nBcc: evil@example.com".to_owned();
    assert!(matches!(serialize(&injected), Err(MimeError::HeaderInjection { header: "From" })));
}

#[test]
fn extra_headers_are_written_in_order_and_stay_closed() {
    let mut with_headers = message();
    with_headers.extra_headers = vec![("Auto-Submitted".to_owned(), "auto-forwarded".to_owned()), ("X-Postino-Rule".to_owned(), "rule_1".to_owned())];
    let serialized = serialize(&with_headers).expect("serialize");
    let raw = raw_text(&serialized);
    assert!(raw.contains("Auto-Submitted: auto-forwarded\r\n"), "{raw}");
    assert!(raw.contains("X-Postino-Rule: rule_1\r\n"), "{raw}");
    assert!(raw.find("Auto-Submitted").zip(raw.find("X-Postino-Rule")).is_some_and(|(left, right)| left < right));

    let mut reserved = message();
    reserved.extra_headers = vec![("Subject".to_owned(), "secondo".to_owned())];
    assert!(matches!(serialize(&reserved), Err(MimeError::Encode { .. })));

    let mut duplicate = message();
    duplicate.extra_headers = vec![("X-Test".to_owned(), "one".to_owned()), ("x-test".to_owned(), "two".to_owned())];
    assert!(matches!(serialize(&duplicate), Err(MimeError::Encode { .. })));

    let mut injected = message();
    injected.extra_headers = vec![("X-Test".to_owned(), "ok\r\nBcc: evil@example.com".to_owned())];
    assert!(matches!(serialize(&injected), Err(MimeError::HeaderInjection { header: "extra header" })));

    let mut too_many = message();
    too_many.extra_headers = (0..9).map(|index| (format!("X-Test-{index}"), "ok".to_owned())).collect();
    assert!(matches!(serialize(&too_many), Err(MimeError::Encode { .. })));
}

#[test]
fn invalid_addresses_and_types_are_actionable() {
    let mut invalid = message();
    invalid.to[0].address = "non-un-indirizzo".to_owned();
    assert!(matches!(serialize(&invalid), Err(MimeError::InvalidAddress { .. })));

    let mut invalid = message();
    invalid.attachments[0].mime = "pdf".to_owned();
    assert!(matches!(serialize(&invalid), Err(MimeError::InvalidMimeType { .. })));

    let mut invalid = message();
    invalid.attachments[0].filename = "..".to_owned();
    assert!(matches!(serialize(&invalid), Err(MimeError::InvalidAttachmentName { .. })));

    let mut invalid = message();
    invalid.subject = "x".repeat(17_000);
    assert!(matches!(serialize(&invalid), Err(MimeError::Encode { .. })));

    let mut invalid = message();
    invalid.references = vec!["a@example.com".to_owned(); 1001];
    assert!(matches!(serialize(&invalid), Err(MimeError::Encode { .. })));
}

#[test]
fn headers_fold_under_78_columns() {
    let mut message = message();
    message.subject = "Un oggetto molto lungo che deve essere spezzato su piu righe senza superare il limite di settantotto colonne".to_owned();
    message.to = (0..6).map(|index| mailbox("Destinatario Con Nome Lungo", &format!("destinatario{index}@example.it"))).collect();
    let raw = raw_text(&serialize(&message).expect("serialize"));
    assert!(raw.contains("\r\n "), "expected a folded header: {raw}");
    for line in raw.split("\r\n") {
        assert!(line.len() <= 78, "{}: {line}", line.len());
    }
    let parsed = parse(raw.as_bytes(), &Limits::default()).expect("parse");
    assert_eq!(parsed.subject, message.subject);
    assert_eq!(parsed.to.len(), 6);
}

#[test]
fn plain_ascii_body_uses_7bit() {
    let mut message = message();
    message.html = None;
    message.attachments.clear();
    message.text = "Ciao Marco,\n\nsolo ASCII.\n".to_owned();
    let raw = raw_text(&serialize(&message).expect("serialize"));
    assert!(raw.contains("Content-Transfer-Encoding: 7bit"), "{raw}");
    let parsed = parse(raw.as_bytes(), &Limits::default()).expect("parse");
    assert_eq!(normalize_newlines(parsed.text.as_deref().expect("text")), normalize_newlines(&message.text));
}

#[test]
fn text_and_attachments_without_html_round_trip() {
    let mut message = message();
    message.html = None;
    let serialized = serialize(&message).expect("serialize");
    let parsed = parse(&serialized.raw, &Limits::default()).expect("parse");
    assert!(parsed.html.is_none());
    assert_eq!(normalize_newlines(parsed.text.as_deref().expect("text")), normalize_newlines(&message.text));
    assert_eq!(parsed.attachments.len(), 1);
    assert_eq!(parsed.attachments[0].data, message.attachments[0].data);
}

#[test]
fn quoted_printable_matches_its_declaration() {
    let mut message = message();
    message.html = None;
    message.attachments.clear();
    let raw = raw_text(&serialize(&message).expect("serialize"));
    assert!(raw.contains("Content-Transfer-Encoding: quoted-printable"), "{raw}");
    assert!(raw.contains("Citt=C3=A0 =C3=A8 pronta."), "{raw}");
    assert!(!raw.contains("multipart/"), "{raw}");
    let parsed = parse(raw.as_bytes(), &Limits::default()).expect("parse");
    assert_eq!(normalize_newlines(parsed.text.as_deref().expect("text")), normalize_newlines(&message.text));
}

#[test]
fn base64_attachment_survives_binary_data() {
    let mut message = message();
    let data = (0..=255u8).collect::<Vec<_>>();
    message.attachments = vec![OutgoingAttachment {
        filename: "dati.bin".to_owned(),
        mime: "application/octet-stream".to_owned(),
        data: data.clone(),
        content_id: None,
    }];
    let serialized = serialize(&message).expect("serialize");
    let raw = raw_text(&serialized);
    assert!(raw.contains("Content-Transfer-Encoding: base64"));
    assert!(raw.contains("Content-Disposition: attachment; filename=\"dati.bin\""));
    let parsed = parse(&serialized.raw, &Limits::default()).expect("parse");
    assert_eq!(parsed.attachments.len(), 1);
    assert_eq!(parsed.attachments[0].data, data);
    assert_eq!(parsed.attachments[0].size, 256);
}

#[test]
fn message_ids_are_generated_or_validated() {
    let generated = serialize(&message()).expect("serialize");
    let raw = raw_text(&generated);
    assert!(raw.contains("Message-ID: <"), "{raw}");
    let parsed = parse(&generated.raw, &Limits::default()).expect("parse");
    let id = parsed.message_id.expect("id");
    assert!(id.ends_with("@example.com"), "{id}");
    assert_eq!(id.matches('@').count(), 1);

    let mut explicit = message();
    explicit.message_id = Some("<custom@example.com>".to_owned());
    let serialized = serialize(&explicit).expect("serialize");
    let raw = raw_text(&serialized);
    assert!(raw.contains("Message-ID: <custom@example.com>"));
    let parsed = parse(&serialized.raw, &Limits::default()).expect("parse");
    assert_eq!(parsed.message_id.as_deref(), Some("custom@example.com"));

    let mut invalid = message();
    invalid.message_id = Some("due parole".to_owned());
    assert!(matches!(serialize(&invalid), Err(MimeError::InvalidMessageId { .. })));
}

#[test]
fn envelope_deduplicates_case_insensitively() {
    let mut message = message();
    message.to = vec![mailbox("", "a@example.com")];
    message.cc = vec![mailbox("", "A@EXAMPLE.COM")];
    message.bcc = vec![mailbox("", "b@example.com")];
    let serialized = serialize(&message).expect("serialize");
    assert_eq!(
        serialized.envelope,
        Envelope { from: "giulia@example.com".to_owned(), recipients: vec!["a@example.com".to_owned(), "b@example.com".to_owned()] }
    );
}

#[test]
fn attachment_names_are_sanitized_and_bounded() {
    let mut message = message();
    let long = format!("{}.pdf", "è".repeat(200));
    message.attachments[0].filename = format!("../../{}", long.replace('/', "_"));
    message.attachments[0].mime = "APPLICATION/PDF".to_owned();
    let serialized = serialize(&message).expect("serialize");
    let raw = raw_text(&serialized);
    assert!(!raw.contains("filename=\"../../"), "{raw}");
    assert!(raw.contains("filename*0*=UTF-8''") || raw.contains("filename*=UTF-8''"), "{raw}");
    let parsed = parse(&serialized.raw, &Limits::default()).expect("parse");
    let filename = &parsed.attachments[0].filename;
    assert!(!filename.contains(['/', '\\']), "{filename}");
    assert!(filename.len() <= 255, "{} bytes: {filename}", filename.len());
    assert_eq!(parsed.attachments[0].mime, "application/pdf");
}

#[test]
fn html_without_attachments_uses_multipart_alternative() {
    let mut message = message();
    message.attachments.clear();
    let serialized = serialize(&message).expect("serialize");
    let raw = raw_text(&serialized);
    let unfolded = raw.replace("\r\n ", " ");
    assert!(unfolded.contains("Content-Type: multipart/alternative; boundary="), "{raw}");
    assert!(!unfolded.contains("multipart/mixed"), "{raw}");
    let parsed = parse(&serialized.raw, &Limits::default()).expect("parse");
    assert_eq!(normalize_newlines(parsed.text.as_deref().expect("text")), normalize_newlines(&message.text));
    let expected_html = sanitize_html(message.html.as_deref().expect("html"), Limits::default().max_html_bytes);
    assert_eq!(parsed.html.as_deref(), Some(expected_html.html.as_str()));
    assert!(parsed.attachments.is_empty());
}

#[test]
fn folded_headers_keep_internal_spaces_and_quoted_names() {
    let mut message = message();
    message.subject = "Due  spazi e un nome con il punto".to_owned();
    message.from.name = Some("Dott. Rossi".to_owned());
    let serialized = serialize(&message).expect("serialize");
    let raw = raw_text(&serialized);
    assert!(raw.contains("From: \"Dott. Rossi\" <giulia@example.com>"), "{raw}");
    let parsed = parse(&serialized.raw, &Limits::default()).expect("parse");
    assert_eq!(parsed.subject, "Due  spazi e un nome con il punto");
    assert_eq!(parsed.from[0].name.as_deref(), Some("Dott. Rossi"));
}

#[test]
fn inline_content_id_round_trips() {
    let mut message = message();
    message.attachments[0].content_id = Some("logo@example.com".to_owned());
    let serialized = serialize(&message).expect("serialize");
    let raw = raw_text(&serialized);
    assert!(raw.contains("Content-Disposition: inline;"));
    assert!(raw.contains("Content-ID: <logo@example.com>"));
    let parsed = parse(&serialized.raw, &Limits::default()).expect("parse");
    assert_eq!(parsed.attachments[0].content_id.as_deref(), Some("logo@example.com"));
    assert_eq!(parsed.attachments[0].disposition, postino_mime::Disposition::Inline);
}
