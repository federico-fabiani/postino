use postino_mime::{Limits, MimeError, Warning, parse};

fn multipart_with(parts: usize) -> Vec<u8> {
    let mut raw = String::from(
        "From: Test <test@example.com>\r\nTo: Dest <dest@example.com>\r\nSubject: molte parti\r\nMIME-Version: 1.0\r\n\
         Content-Type: multipart/mixed; boundary=\"b\"\r\n\r\n",
    );
    for index in 0..parts {
        raw.push_str(&format!("--b\r\nContent-Type: text/plain; charset=utf-8\r\n\r\nparte {index}\r\n"));
    }
    raw.push_str("--b--\r\n");
    raw.into_bytes()
}

fn nested_multiparts(depth: usize) -> Vec<u8> {
    let mut raw = String::from(
        "From: Test <test@example.com>\r\nTo: Dest <dest@example.com>\r\nSubject: annidato\r\nMIME-Version: 1.0\r\n\
         Content-Type: multipart/mixed; boundary=\"b0\"\r\n\r\n",
    );
    for index in 0..depth {
        raw.push_str(&format!("--b{index}\r\nContent-Type: multipart/mixed; boundary=\"b{}\"\r\n\r\n", index + 1));
    }
    raw.push_str(&format!("--b{depth}\r\nContent-Type: text/plain; charset=utf-8\r\n\r\nciao\r\n"));
    for index in (0..=depth).rev() {
        raw.push_str(&format!("--b{index}--\r\n"));
    }
    raw.into_bytes()
}

fn attachment_message(data: &[u8]) -> Vec<u8> {
    let encoded = postino_mime_test_base64(data);
    format!(
        "From: Test <test@example.com>\r\nTo: Dest <dest@example.com>\r\nSubject: allegato\r\nMIME-Version: 1.0\r\n\
         Content-Type: multipart/mixed; boundary=\"b\"\r\n\r\n\
         --b\r\nContent-Type: text/plain; charset=utf-8\r\n\r\ncorpo\r\n\
         --b\r\nContent-Type: application/octet-stream\r\nContent-Disposition: attachment; filename=\"dati.bin\"\r\n\
         Content-Transfer-Encoding: base64\r\n\r\n{encoded}\r\n--b--\r\n"
    )
    .into_bytes()
}

fn postino_mime_test_base64(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in data.chunks(3) {
        let a = chunk.first().copied().unwrap_or(0);
        let b = chunk.get(1).copied().unwrap_or(0);
        let c = chunk.get(2).copied().unwrap_or(0);
        let triple = (u32::from(a) << 16) | (u32::from(b) << 8) | u32::from(c);
        let digits = [(triple >> 18) & 0x3f, (triple >> 12) & 0x3f, (triple >> 6) & 0x3f, triple & 0x3f];
        let keep = chunk.len().min(3) + 1;
        for (index, digit) in digits.iter().enumerate() {
            if index < keep {
                out.push(char::from(*ALPHABET.get(*digit as usize).unwrap_or(&b'A')));
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[test]
fn rejects_raw_input_over_the_limit() {
    let limits = Limits { max_bytes: 100, ..Limits::default() };
    let error = parse(&[b'a'; 200], &limits).expect_err("too large");
    assert!(matches!(error, MimeError::TooLarge { size: 200, limit: 100 }), "{error:?}");
}

#[test]
fn rejects_a_header_block_over_the_limit() {
    let mut raw = String::from("X-Long: ");
    raw.push_str(&"a".repeat(200));
    raw.push_str("\r\n\r\nbody");
    let limits = Limits { max_header_bytes: 100, ..Limits::default() };
    let error = parse(raw.as_bytes(), &limits).expect_err("header too large");
    assert!(matches!(error, MimeError::TooLarge { .. }), "{error:?}");
}

#[test]
fn part_limit_is_recorded_not_fatal() {
    let raw = multipart_with(30);
    let limits = Limits { max_parts: 4, max_depth: 8, ..Limits::default() };
    let message = parse(&raw, &limits).expect("bounded parse");
    assert!(message.warnings.contains(&Warning::TooManyParts), "{:?}", message.warnings);
}

#[test]
fn excessive_multipart_headers_are_refused_before_parsing() {
    let raw = nested_multiparts(40);
    let limits = Limits { max_depth: 4, max_parts: 512, ..Limits::default() };
    let error = parse(&raw, &limits).expect_err("too deep");
    assert!(matches!(error, MimeError::TooDeep { limit: 4 }), "{error:?}");
}

#[test]
fn nesting_at_the_parser_limit_is_recorded() {
    let raw = nested_multiparts(10);
    let limits = Limits { max_depth: 4, max_parts: 512, ..Limits::default() };
    let message = parse(&raw, &limits).expect("bounded parse");
    assert!(message.warnings.contains(&Warning::TooDeep) || message.warnings.contains(&Warning::TooManyParts), "{:?}", message.warnings);
}

#[test]
fn oversized_attachment_data_is_dropped_but_metadata_survives() {
    let raw = attachment_message(&[b'x'; 100]);
    let limits = Limits { max_part_bytes: 10, ..Limits::default() };
    let message = parse(&raw, &limits).expect("parse");
    assert_eq!(message.attachments.len(), 1);
    let attachment = &message.attachments[0];
    assert_eq!(attachment.size, 100);
    assert!(attachment.data.is_empty());
    assert!(attachment.truncated);
    assert!(message.warnings.contains(&Warning::PartTooLarge));
    assert!(message.warnings.contains(&Warning::Truncated));
}

#[test]
fn total_attachment_budget_stops_after_the_first_part() {
    let first = postino_mime_test_base64(&[b'x'; 100]);
    let second = postino_mime_test_base64(&[b'y'; 100]);
    let raw = format!(
        "From: Test <test@example.com>\r\nTo: Dest <dest@example.com>\r\nSubject: allegati\r\nMIME-Version: 1.0\r\n\
         Content-Type: multipart/mixed; boundary=\"b\"\r\n\r\n\
         --b\r\nContent-Type: text/plain; charset=utf-8\r\n\r\ncorpo\r\n\
         --b\r\nContent-Type: application/octet-stream\r\nContent-Disposition: attachment; filename=\"uno.bin\"\r\n\
         Content-Transfer-Encoding: base64\r\n\r\n{first}\r\n\
         --b\r\nContent-Type: application/octet-stream\r\nContent-Disposition: attachment; filename=\"due.bin\"\r\n\
         Content-Transfer-Encoding: base64\r\n\r\n{second}\r\n--b--\r\n"
    )
    .into_bytes();
    let limits = Limits { max_total_attachment_bytes: 150, ..Limits::default() };
    let message = parse(&raw, &limits).expect("parse");
    assert_eq!(message.attachments.len(), 2, "{:?}", message.attachments);
    assert_eq!(message.attachments[0].data.len(), 100);
    assert!(!message.attachments[0].truncated);
    assert!(message.attachments[1].data.is_empty());
    assert!(message.attachments[1].truncated);
    assert!(message.warnings.contains(&Warning::TotalAttachmentLimit));
}

#[test]
fn nested_messages_are_metadata_only() {
    let nested = "From: Interno <interno@example.com>\r\nSubject: dentro\r\n\r\ncorpo interno\r\n";
    let encoded = postino_mime_test_base64(nested.as_bytes());
    let raw = format!(
        "From: Test <test@example.com>\r\nTo: Dest <dest@example.com>\r\nSubject: inoltro\r\nMIME-Version: 1.0\r\n\
         Content-Type: multipart/mixed; boundary=\"b\"\r\n\r\n\
         --b\r\nContent-Type: text/plain; charset=utf-8\r\n\r\ncorpo\r\n\
         --b\r\nContent-Type: message/rfc822\r\nContent-Disposition: attachment; filename=\"inoltrato.eml\"\r\n\
         Content-Transfer-Encoding: base64\r\n\r\n{encoded}\r\n--b--\r\n"
    )
    .into_bytes();
    let message = parse(&raw, &Limits::default()).expect("parse");
    let attachment = message.attachments.iter().find(|attachment| attachment.filename == "inoltrato.eml").expect("nested message");
    assert_eq!(attachment.mime, "message/rfc822");
    assert!(attachment.data.is_empty(), "nested message bytes must not be materialised");
    assert!(attachment.truncated);
    assert_eq!(attachment.size, nested.len() as u64);
}

#[test]
fn html_output_is_capped_and_recorded() {
    let limits = Limits { max_html_bytes: 60, ..Limits::default() };
    let message = parse(include_bytes!("fixtures/newsletter.eml"), &limits).expect("parse");
    let html = message.html.expect("html");
    assert!(html.len() <= 60, "{} bytes: {html}", html.len());
    assert!(message.warnings.contains(&Warning::Truncated));
}

#[test]
fn hostile_limits_are_clamped() {
    let absurd = Limits {
        max_bytes: usize::MAX,
        max_parts: 0,
        max_depth: 0,
        max_part_bytes: usize::MAX,
        max_total_attachment_bytes: usize::MAX,
        max_html_bytes: usize::MAX,
        max_header_bytes: usize::MAX,
    };
    let parsed = absurd.clamped();
    assert_eq!(parsed.max_bytes, Limits::HARD_MAX_BYTES);
    assert_eq!(parsed.max_parts, 1);
    assert_eq!(parsed.max_depth, 1);
    assert!(parse(include_bytes!("fixtures/simple.eml"), &absurd).is_ok());
}
