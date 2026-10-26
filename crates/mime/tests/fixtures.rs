use postino_mime::{Disposition, Limits, ParsedMessage, Warning, parse};

fn load(raw: &'static [u8]) -> ParsedMessage {
    parse(raw, &Limits::default()).expect("fixture must parse")
}

fn warnings(message: &ParsedMessage) -> Vec<Warning> {
    message.warnings.clone()
}

#[test]
fn simple_text_plain() {
    let message = load(include_bytes!("fixtures/simple.eml"));
    assert_eq!(message.subject, "Preventivo per il progetto");
    assert_eq!(message.from.len(), 1);
    assert_eq!(message.from[0].name.as_deref(), Some("Giulia Ferri"));
    assert_eq!(message.from[0].address, "giulia@example.com");
    assert_eq!(message.to[0].address, "marco@example.it");
    assert_eq!(message.date.expect("date").to_rfc3339(), "2026-10-25T09:15:00+00:00");
    assert_eq!(message.message_id.as_deref(), Some("simple-001@example.com"));
    let text = message.text.as_deref().expect("text");
    assert!(text.contains("preventivo aggiornato"), "{text}");
    assert!(message.html.is_none());
    assert!(message.attachments.is_empty());
    assert!(!message.is_multipart);
    assert!(!message.has_calendar);
    assert!(message.warnings.is_empty(), "{:?}", message.warnings);
    assert_eq!(message.headers.get("subject"), Some("Preventivo per il progetto"));
}

#[test]
fn alternative_prefers_plain_and_sanitizes_html() {
    let message = load(include_bytes!("fixtures/alternative.eml"));
    assert_eq!(message.subject, "Aggiornamento città");
    let text = message.text.as_deref().expect("text");
    assert!(text.contains("Città, aggiornamento pronto."), "{text}");
    let html = message.html.as_deref().expect("html");
    assert!(html.contains("<strong>aggiornamento</strong>"), "{html}");
    assert!(!html.contains("<html"), "{html}");
    assert!(!html.contains("<body"), "{html}");
    assert!(message.attachments.is_empty(), "{:?}", message.attachments);
    assert!(message.is_multipart);
    assert!(!warnings(&message).contains(&Warning::BadCharset), "{:?}", message.warnings);
}

#[test]
fn newsletter_blocks_remote_images_and_converts_text() {
    let message = load(include_bytes!("fixtures/newsletter.eml"));
    assert_eq!(message.subject, "Notizie di ottobre");
    assert_eq!(message.remote_images_blocked, 2);
    let html = message.html.as_deref().expect("html");
    assert!(html.contains("<h1>Notizie di ottobre</h1>"), "{html}");
    assert!(html.contains("<table>"), "{html}");
    assert!(html.contains("rel=\"noopener noreferrer\""), "{html}");
    assert!(!html.contains("<script"), "{html}");
    assert!(!html.contains("alert"), "{html}");
    assert!(!html.contains("tracker.example"), "{html}");
    assert!(!html.contains("cdn.example.com"), "{html}");
    assert!(!html.contains("<img"), "{html}");
    assert!(!html.contains("<style"), "{html}");
    let text = message.text.as_deref().expect("text");
    assert!(text.contains("Notizie di ottobre"), "{text}");
    assert!(text.contains("- Prima novità"), "{text}");
    assert!(text.contains("Leggi tutto <https://example.com/leggi>"), "{text}");
    assert!(!text.contains("<p>"), "{text}");
    assert!(!text.contains("</"), "{text}");
    assert!(warnings(&message).contains(&Warning::RemoteContentBlocked));
    assert!(warnings(&message).contains(&Warning::SanitizerStripped));
    assert!(message.attachments.is_empty());
    assert!(!message.is_multipart);
}

#[test]
fn remote_images_are_counted_without_losing_the_inline_cid() {
    let message = load(include_bytes!("fixtures/remote_images.eml"));
    assert_eq!(message.remote_images_blocked, 2, "the parser must count exactly the two http(s) images");
    let html = message.html.as_deref().expect("html");
    assert!(html.contains("src=\"cid:logo@example.com\""), "the inline cid image must stay: {html}");
    assert!(!html.contains("cdn.example.com"), "{html}");
    assert!(!html.contains("tracker.example.com"), "{html}");
    assert!(!html.contains("http://") && !html.contains("https://"), "a remote source survived: {html}");
    assert!(warnings(&message).contains(&Warning::RemoteContentBlocked));
}

#[test]
fn nested_mixed_keeps_bodies_and_attachments() {
    let message = load(include_bytes!("fixtures/nested.eml"));
    assert!(message.is_multipart);
    assert_eq!(message.attachments.len(), 2, "{:?}", message.attachments);
    let text = message.text.as_deref().expect("text");
    assert!(text.contains("in allegato il preventivo"), "{text}");
    let html = message.html.as_deref().expect("html");
    assert!(html.contains("src=\"cid:image1@example.com\""), "{html}");

    let pdf = message.attachments.iter().find(|attachment| attachment.filename == "Preventivo.pdf").expect("pdf");
    assert_eq!(pdf.mime, "application/pdf");
    assert_eq!(pdf.disposition, Disposition::Attachment);
    assert_eq!(pdf.size, 14);
    assert_eq!(pdf.data, b"%PDF-1.4\n%EOF\n");
    assert!(!pdf.truncated);
    assert!(pdf.content_id.is_none());

    let png = message.attachments.iter().find(|attachment| attachment.filename == "logo.png").expect("png");
    assert_eq!(png.mime, "image/png");
    assert_eq!(png.disposition, Disposition::Inline);
    assert_eq!(png.content_id.as_deref(), Some("image1@example.com"));
    assert!(png.data.starts_with(b"\x89PNG\r\n\x1a\n"), "{:?}", png.data);
    assert!(!png.truncated);

    // Neither chosen body may be repeated as an attachment.
    assert!(message.attachments.iter().all(|attachment| !attachment.filename.ends_with(".txt")));
    assert!(message.attachments.iter().all(|attachment| attachment.filename != "attachment.html"));
}

#[test]
fn encoded_headers_decode_rfc2047() {
    let message = load(include_bytes!("fixtures/encoded_headers.eml"));
    assert_eq!(message.subject, "Città 👋 e preventivo");
    assert_eq!(message.from[0].name.as_deref(), Some("Città Servizi"));
    assert_eq!(message.to[0].name.as_deref(), Some("Giulia Ferri"));
    assert_eq!(message.to[1].name.as_deref(), Some("Marco, Verdi"));
    assert!(message.text.as_deref().expect("text").contains("il preventivo è pronto"));
}

#[test]
fn rfc2231_decodes_continuation_and_charset() {
    let message = load(include_bytes!("fixtures/rfc2231.eml"));
    assert_eq!(message.attachments.len(), 1);
    let attachment = &message.attachments[0];
    assert_eq!(attachment.filename, "Relazione finale dell'ufficio tecnico € pronta.pdf");
    assert_eq!(attachment.mime, "application/pdf");
    assert_eq!(attachment.data, b"%PDF-1.4\n%EOF\n");
}

#[test]
fn quoted_printable_body_and_base64_attachment() {
    let message = load(include_bytes!("fixtures/quoted_printable.eml"));
    assert_eq!(message.subject, "Città e relazione");
    let text = message.text.as_deref().expect("text");
    assert!(text.contains("Città, ecco l'aggiornamento:"), "{text}");
    assert!(text.contains("- voce uno"), "{text}");
    assert_eq!(message.attachments.len(), 1);
    assert_eq!(message.attachments[0].filename, "relazione.pdf");
    assert_eq!(message.attachments[0].data, b"%PDF-1.4\n%EOF\n");
    assert!(!message.warnings.contains(&Warning::MalformedTransferEncoding));
}

#[test]
fn xss_corpus_is_dead() {
    let message = load(include_bytes!("fixtures/xss.eml"));
    let html = message.html.as_deref().expect("html");
    let lower = html.to_ascii_lowercase();
    for forbidden in [
        "<script",
        "javascript:",
        "vbscript:",
        "data:",
        "onerror",
        "onclick",
        "style=",
        "<iframe",
        "<form",
        "<input",
        "<button",
        "<svg",
        "<img",
        "evil.example",
        "href=\"//",
    ] {
        assert!(!lower.contains(forbidden), "{forbidden} survived: {html}");
    }
    assert!(html.contains("Testo sicuro"), "{html}");
    assert!(html.contains("rel=\"noopener noreferrer\""));
    assert!(message.text.as_deref().expect("text").contains("Testo sicuro"));
    assert!(warnings(&message).contains(&Warning::SanitizerStripped));
    assert_eq!(message.remote_images_blocked, 0);
}

#[test]
fn malformed_message_is_tolerated() {
    let message = load(include_bytes!("fixtures/malformed.eml"));
    assert!(message.subject.starts_with("Malformato"), "{:?}", message.subject);
    assert!(!message.subject.chars().any(char::is_control), "{:?}", message.subject);
    let text = message.text.as_deref().expect("text");
    assert!(text.contains("charset sconosciuto"), "{text}");
    let expected = [Warning::BadCharset, Warning::MalformedTransferEncoding, Warning::MissingBoundary];
    for warning in expected {
        assert!(warnings(&message).contains(&warning), "missing {warning:?}: {:?}", message.warnings);
    }
    let attachment = message.attachments.iter().find(|attachment| attachment.filename == "troncato.pdf").expect("truncated pdf");
    assert!(attachment.data.starts_with(b"%PDF-1.4"));
    assert!(attachment.size >= attachment.data.len() as u64);
}

#[test]
fn injection_fixture_keeps_controls_out_of_fields() {
    let message = load(include_bytes!("fixtures/injection.eml"));
    assert!(message.bcc.is_empty(), "{:?}", message.bcc);
    assert!(!message.subject.chars().any(char::is_control), "{:?}", message.subject);
    assert!(message.subject.contains("Bcc: evil@example.com"), "{:?}", message.subject);
    let from_name = message.from[0].name.as_deref().expect("display name");
    assert!(!from_name.chars().any(char::is_control), "{from_name:?}");
    assert!(from_name.contains("Città"), "{from_name:?}");
    let attachment = &message.attachments[0];
    assert!(!attachment.filename.chars().any(char::is_control), "{:?}", attachment.filename);
    assert!(!attachment.filename.contains(['/', '\\']), "{:?}", attachment.filename);
    assert!(attachment.filename.contains("Relazione"), "{:?}", attachment.filename);
}
