use postino_mime::{Limits, Warning, parse as parse_message};

fn parse(raw: &[u8]) -> postino_mime::ParsedMessage {
    parse_message(raw, &Limits::default()).expect("parse")
}

#[test]
fn missing_content_type_is_text_plain() {
    let message = parse(b"From: Test <test@example.com>\r\nSubject: senza tipo\r\n\r\ncorpo semplice\r\n");
    assert_eq!(message.text.as_deref(), Some("corpo semplice\r\n"));
    assert!(message.html.is_none());
    assert!(message.attachments.is_empty());
    assert!(!message.is_multipart);
}

#[test]
fn unknown_transfer_encoding_becomes_an_attachment() {
    let raw = concat!(
        "From: Test <test@example.com>\r\n",
        "To: Dest <dest@example.com>\r\n",
        "Subject: codifica sconosciuta\r\n",
        "Content-Type: multipart/mixed; boundary=\"b\"\r\n",
        "\r\n",
        "--b\r\n",
        "Content-Type: text/plain; charset=utf-8\r\n",
        "\r\n",
        "corpo leggibile\r\n",
        "--b\r\n",
        "Content-Type: text/plain; charset=utf-8\r\n",
        "Content-Disposition: attachment; filename=\"strano.txt\"\r\n",
        "Content-Transfer-Encoding: x-uuencode\r\n",
        "\r\n",
        "inizio dati strani\r\n",
        "--b--\r\n"
    );
    let message = parse(raw.as_bytes());
    assert_eq!(message.text.as_deref(), Some("corpo leggibile"));
    assert_eq!(message.attachments.len(), 1);
    let attachment = &message.attachments[0];
    assert_eq!(attachment.filename, "strano.txt");
    assert_eq!(attachment.mime, "text/plain");
    assert!(attachment.data.starts_with(b"inizio dati strani"));
    assert!(message.warnings.contains(&Warning::UnknownEncoding));
}

#[test]
fn an_empty_address_list_keeps_the_raw_header() {
    let message = parse(b"From: Test <test@example.com>\r\nTo: non-un-indirizzo\r\nSubject: destinatario rotto\r\n\r\ncorpo\r\n");
    assert!(message.to.is_empty());
    assert_eq!(message.headers.get("to"), Some("non-un-indirizzo"));
    assert!(message.warnings.contains(&Warning::BadAddress));
}

#[test]
fn groups_expand_to_their_members() {
    let message =
        parse(b"From: Test <test@example.com>\r\nTo: team: a@example.com, b@example.com;, c@example.com\r\nSubject: gruppo\r\n\r\ncorpo\r\n");
    let addresses = message.to.iter().map(|mailbox| mailbox.address.as_str()).collect::<Vec<_>>();
    assert_eq!(addresses, ["a@example.com", "b@example.com", "c@example.com"]);
    assert!(!message.warnings.contains(&Warning::BadAddress));
}

#[test]
fn calendar_parts_are_attachments_and_flagged() {
    let raw = concat!(
        "From: Test <test@example.com>\r\n",
        "To: Dest <dest@example.com>\r\n",
        "Subject: invito\r\n",
        "Content-Type: multipart/mixed; boundary=\"b\"\r\n",
        "\r\n",
        "--b\r\n",
        "Content-Type: text/plain; charset=utf-8\r\n",
        "\r\n",
        "invito allegato\r\n",
        "--b\r\n",
        "Content-Type: text/calendar; charset=utf-8\r\n",
        "Content-Disposition: attachment; filename=\"invito.ics\"\r\n",
        "\r\n",
        "BEGIN:VCALENDAR\r\nEND:VCALENDAR\r\n",
        "--b--\r\n"
    );
    let message = parse(raw.as_bytes());
    assert!(message.has_calendar);
    assert!(message.attachments.iter().any(|attachment| attachment.mime == "text/calendar" && attachment.filename == "invito.ics"));
    assert_eq!(message.text.as_deref(), Some("invito allegato"));
}
