//! P12d: calendar parts are bounded before and after the third-party parser.
//!
//! These fixtures are synthetic and hostile on purpose. A refused part is a normal attachment, so
//! every case here asserts either a bounded value or a typed error, never a panic.

use postino_mime::{InviteError, MAX_CALENDAR_BYTES, MAX_CALENDAR_COMPONENTS, MAX_INVITE_ATTENDEES, parse_invite};

fn request(extra: &str) -> String {
    format!(
        "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nMETHOD:REQUEST\r\nBEGIN:VEVENT\r\nUID:cap-1@example.com\r\nDTSTAMP:20261024T084200Z\r\nDTSTART:20261105T090000Z\r\nDTEND:20261105T100000Z\r\nSUMMARY:Revisione\r\nORGANIZER:mailto:giulia@example.com\r\n{extra}END:VEVENT\r\nEND:VCALENDAR\r\n"
    )
}

#[test]
fn size_component_and_attendee_bounds_are_enforced() {
    assert_eq!(parse_invite(&vec![b'A'; MAX_CALENDAR_BYTES + 1]), Err(InviteError::TooLarge));

    let max = request("");
    let pad = MAX_CALENDAR_BYTES - max.len() - "X-PAD:\r\n".len();
    let exact = max.replace("END:VEVENT", &format!("X-PAD:{}\r\nEND:VEVENT", "A".repeat(pad)));
    assert_eq!(exact.len(), MAX_CALENDAR_BYTES);
    assert!(parse_invite(exact.as_bytes()).is_ok(), "exactly 256 KiB is inside the cap");
    assert_eq!(parse_invite(format!("{exact} ").as_bytes()), Err(InviteError::TooLarge));

    let mut components =
        String::from("BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:cap@example.com\r\nDTSTART:20261105T090000Z\r\nSUMMARY:Revisione\r\n");
    components.push_str("END:VEVENT\r\n");
    for _ in 0..MAX_CALENDAR_COMPONENTS - 2 {
        components.push_str("BEGIN:X\r\nEND:X\r\n");
    }
    components.push_str("END:VCALENDAR\r\n");
    assert!(parse_invite(components.as_bytes()).is_ok(), "exactly 1000 components are accepted");
    let mut over = components.clone();
    over.insert_str(over.len() - "END:VCALENDAR\r\n".len(), "BEGIN:X\r\n");
    assert_eq!(parse_invite(over.as_bytes()), Err(InviteError::TooManyComponents), "1001 components exceed the cap");
    let mut mixed = String::from("BeGiN:VCALENDAR\r\n");
    for _ in 0..MAX_CALENDAR_COMPONENTS {
        mixed.push_str("bEgIn:X\r\nEnD:X\r\n");
    }
    assert_eq!(parse_invite(mixed.as_bytes()), Err(InviteError::TooManyComponents), "case must not hide BEGIN");

    let mut attendees = String::new();
    for index in 0..MAX_INVITE_ATTENDEES {
        attendees.push_str(&format!("ATTENDEE:mailto:person{index}@example.com\r\n"));
    }
    let exact_attendees = parse_invite(request(&attendees).as_bytes()).expect("exactly 2000 attendees");
    assert_eq!(exact_attendees.attendees.len(), MAX_INVITE_ATTENDEES);
    assert!(!exact_attendees.warnings.contains(&postino_mime::InviteWarning::AttendeesTruncated));
    attendees.push_str("ATTENDEE:mailto:extra@example.com\r\n");
    let over_attendees = parse_invite(request(&attendees).as_bytes()).expect("bounded invite");
    assert_eq!(over_attendees.attendees.len(), MAX_INVITE_ATTENDEES);
    assert!(over_attendees.warnings.contains(&postino_mime::InviteWarning::AttendeesTruncated));
}

#[test]
fn malformed_and_hostile_input_never_panics_and_never_becomes_a_card() {
    let cases: [&[u8]; 4] = [
        b"",
        b"BEGIN:VCALENDAR\r\n",
        b"\xff\xfe\x00\x00",
        b"BEGIN:VCALENDAR\r\nMETHOD:REQUEST\r\nBEGIN:VTODO\r\nUID:x\r\nEND:VTODO\r\nEND:VCALENDAR\r\n",
    ];
    for (index, case) in cases.iter().enumerate() {
        let parsed = std::panic::catch_unwind(|| parse_invite(case));
        assert!(parsed.is_ok(), "case {index}: the parser must not panic");
        assert!(parsed.expect("no panic").is_err(), "case {index}: a malformed part must fall back to the attachment");
    }
    let bad_date = parse_invite(
        b"BEGIN:VCALENDAR\r\nMETHOD:REQUEST\r\nBEGIN:VEVENT\r\nUID:x\r\nDTSTART:not-a-date\r\nSUMMARY:x\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n",
    )
    .expect("a structurally readable event keeps a card");
    assert!(!bad_date.valid && !bad_date.can_reply, "an unreadable date disables the reply");

    let hostile = parse_invite(
        b"BEGIN:VCALENDAR\r\nMETHOD:REQUEST\r\nBEGIN:VEVENT\r\nUID:x\r\nDTSTART:20261105T090000Z\r\nSUMMARY:Cena\r\nBcc: attacker@example.com\r\nORGANIZER:mailto:giulia@example.com\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n",
    )
    .expect("a card without the injected field");
    assert!(!hostile.summary.contains("Bcc:"));
    assert!(!hostile.attendees.iter().any(|attendee| attendee.contains("attacker")));
}

#[test]
fn request_cancel_and_reply_are_distinguishable() {
    let accepted = parse_invite(request("").as_bytes()).expect("request");
    assert_eq!(accepted.method, "REQUEST");
    assert!(accepted.can_reply && !accepted.cancelled);
    let cancel = parse_invite(request("").replace("METHOD:REQUEST", "METHOD:CANCEL").as_bytes()).expect("cancel");
    assert!(cancel.cancelled && !cancel.can_reply);
    let reply = parse_invite(request("").replace("METHOD:REQUEST", "METHOD:REPLY").as_bytes()).expect("reply");
    assert_eq!(reply.method, "REPLY");
    assert!(!reply.can_reply);
}
