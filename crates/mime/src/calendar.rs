//! Safe, parse-only support for received `text/calendar` invites (P12d).
//!
//! The parser is deliberately narrow: one `VEVENT`, no recurrence expansion and no timezone
//! database. It never trusts the input: the part size, the component count, the attendee count and
//! every displayed field are bounded, and a panic inside the third-party parser is caught and
//! reported as a malformed invite. Anything this module refuses stays a normal attachment.

use chrono::{DateTime, NaiveDate, NaiveDateTime, Utc};
use icalendar::{Calendar, Component};
use std::fmt;

/// Hard cap on the calendar part. A larger part stays a normal attachment.
pub const MAX_CALENDAR_BYTES: usize = 256 * 1024;
/// Hard cap on `BEGIN:` components in one calendar part.
pub const MAX_CALENDAR_COMPONENTS: usize = 1_000;
/// Hard cap on organizer-visible attendees; the rest are truncated with a warning.
pub const MAX_INVITE_ATTENDEES: usize = 2_000;
/// Cap for summary, location, organizer and attendee text, in characters.
pub const MAX_INVITE_FIELD_CHARS: usize = 64;
/// Cap for one re-emitted property line (name, parameters and value).
const MAX_INVITE_LINE_CHARS: usize = 512;

/// Why a card is shown with a warning.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InviteWarning {
    /// `RRULE`, `RDATE` or `EXDATE`: only the first occurrence is shown.
    RecurrenceUnsupported,
    /// The event declares a `TZID`; the wall clock in the invite is shown as written.
    TimezoneUnsupported,
    /// More than [`MAX_INVITE_ATTENDEES`] attendees: the list was truncated.
    AttendeesTruncated,
    /// A display field was longer than [`MAX_INVITE_FIELD_CHARS`] and was shortened.
    FieldsTruncated,
    /// More than one event: only the first is shown.
    MultipleEvents,
}

impl InviteWarning {
    /// Stable machine-readable name, also used by the core model.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::RecurrenceUnsupported => "recurrence_unsupported",
            Self::TimezoneUnsupported => "timezone_unsupported",
            Self::AttendeesTruncated => "attendees_truncated",
            Self::FieldsTruncated => "fields_truncated",
            Self::MultipleEvents => "multiple_events",
        }
    }

    /// One Italian sentence for the reading pane.
    pub const fn message(self) -> &'static str {
        match self {
            Self::RecurrenceUnsupported => "Ricorrenza non supportata: viene mostrata solo la prima occorrenza.",
            Self::TimezoneUnsupported => "Fuso orario dichiarato ma non applicato: gli orari sono quelli scritti nell'invito.",
            Self::AttendeesTruncated => "Elenco partecipanti troncato a 2000 voci.",
            Self::FieldsTruncated => "Alcuni campi sono stati accorciati a 64 caratteri.",
            Self::MultipleEvents => "L'invito contiene più eventi: viene mostrato solo il primo.",
        }
    }
}

/// A parsed invite. It is a value only: nothing is stored and nothing is sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CalendarInvite {
    /// Uppercased `METHOD`, or an empty string when the calendar does not declare one.
    pub method: String,
    /// The event `UID`, echoed by the `METHOD:REPLY` calendar.
    pub uid: String,
    /// The event title, capped at [`MAX_INVITE_FIELD_CHARS`] characters.
    pub summary: String,
    /// The untruncated title. Only the `METHOD:REPLY` calendar uses it; the card shows
    /// [`Self::summary`].
    pub summary_full: String,
    /// Organizer email address without the `mailto:` prefix.
    pub organizer: Option<String>,
    /// Attendee email addresses without the `mailto:` prefix, capped and de-duplicated.
    pub attendees: Vec<String>,
    /// Normalized ISO 8601 display value (`2026-11-05T09:00:00Z`, `...T09:00:00` or `2026-11-05`).
    pub start: Option<String>,
    /// Normalized end value, when the event declares `DTEND`.
    pub end: Option<String>,
    /// The original `DTSTART`/`DTEND` values, needed to build the `METHOD:REPLY` part.
    pub start_raw: Option<String>,
    /// The original `DTEND` value.
    pub end_raw: Option<String>,
    /// The original `DTSTART` property line, parameters included (`DTSTART;TZID=...:...`).
    pub start_line: Option<String>,
    /// The original `DTEND` property line, parameters included.
    pub end_line: Option<String>,
    /// `SEQUENCE` as a non-negative integer; `0` when absent or unreadable.
    pub sequence: u32,
    /// The event location, capped like the summary.
    pub location: Option<String>,
    /// The invite declares a recurrence rule or explicit recurrence dates.
    pub recurrence: bool,
    /// The `TZID` declared by `DTSTART`/`DTEND`, if any.
    pub timezone: Option<String>,
    /// The event has the fields a reply needs: UID, summary and a parseable start.
    pub valid: bool,
    /// Why the event is not valid, in a short sentence.
    pub invalid_reason: Option<String>,
    /// `METHOD:CANCEL` or `STATUS:CANCELLED`.
    pub cancelled: bool,
    /// Reply is offered only for a valid `REQUEST` with an organizer.
    pub can_reply: bool,
    /// Warnings shown on the card, deduplicated by construction.
    pub warnings: Vec<InviteWarning>,
}

/// Why the part is not rendered as a card.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InviteError {
    /// The part is larger than [`MAX_CALENDAR_BYTES`].
    TooLarge,
    /// The input is not valid UTF-8.
    NotUtf8,
    /// The calendar is malformed or the parser panicked.
    Malformed,
    /// The part has no `VEVENT`.
    NoEvent,
    /// The part has more than [`MAX_CALENDAR_COMPONENTS`] `BEGIN:` components.
    TooManyComponents,
}

impl fmt::Display for InviteError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            Self::TooLarge => "the calendar part is larger than 256 KiB",
            Self::NotUtf8 => "the calendar part is not UTF-8",
            Self::Malformed => "the calendar part is malformed",
            Self::NoEvent => "the calendar part has no VEVENT",
            Self::TooManyComponents => "the calendar part has more than 1000 components",
        };
        formatter.write_str(text)
    }
}

impl std::error::Error for InviteError {}

/// Parse a `text/calendar` payload. It never panics: a panic inside the third-party parser is
/// reported as [`InviteError::Malformed`].
pub fn parse_invite(bytes: &[u8]) -> Result<CalendarInvite, InviteError> {
    if bytes.len() > MAX_CALENDAR_BYTES {
        return Err(InviteError::TooLarge);
    }
    let text = std::str::from_utf8(bytes).map_err(|_| InviteError::NotUtf8)?;
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    if text.as_bytes().windows(6).filter(|window| window.eq_ignore_ascii_case(b"BEGIN:")).count() > MAX_CALENDAR_COMPONENTS {
        return Err(InviteError::TooManyComponents);
    }
    let parsed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| text.parse::<Calendar>()))
        .map_err(|_| InviteError::Malformed)?
        .map_err(|_| InviteError::Malformed)?;
    invite_from_calendar(&parsed)
}

fn invite_from_calendar(calendar: &Calendar) -> Result<CalendarInvite, InviteError> {
    let events = calendar.events().collect::<Vec<_>>();
    let Some(first) = events.first().copied() else {
        return Err(InviteError::NoEvent);
    };
    // A calendar may carry an override with a RECURRENCE-ID before the master; prefer the first
    // event that has a DTSTART so the card is never blank for a harmless ordering difference.
    let event = events.iter().copied().find(|event| event.property_value("DTSTART").is_some()).unwrap_or(first);

    let mut warnings = Vec::new();
    if events.len() > 1 {
        warnings.push(InviteWarning::MultipleEvents);
    }

    let method = calendar.property_value("METHOD").unwrap_or_default().trim().to_ascii_uppercase();
    let status = event.property_value("STATUS").unwrap_or_default().trim().to_ascii_uppercase();
    let cancelled = method == "CANCEL" || status == "CANCELLED";

    let raw_summary = event.property_value("SUMMARY").unwrap_or_default();
    let summary_full = clean_field(raw_summary);
    let (summary, summary_cut) = bounded_field(raw_summary);
    let (location, location_cut) = bounded_field(event.property_value("LOCATION").unwrap_or_default());
    let (organizer, organizer_cut) = event.property_value("ORGANIZER").map(email_from_calendar_value).unwrap_or((None, false));
    let (attendees, truncated, attendee_cut) = attendees_of(event);
    if summary_cut || location_cut || organizer_cut || attendee_cut {
        warnings.push(InviteWarning::FieldsTruncated);
    }
    if truncated {
        warnings.push(InviteWarning::AttendeesTruncated);
    }

    let start_property = event.properties().get("DTSTART");
    let end_property = event.properties().get("DTEND");
    let timezone = start_property
        .and_then(|property| property.get_param_as("TZID", |value| Some(value.to_owned())))
        .or_else(|| end_property.and_then(|property| property.get_param_as("TZID", |value| Some(value.to_owned()))));
    if timezone.is_some() {
        warnings.push(InviteWarning::TimezoneUnsupported);
    }

    let start_raw = start_property.map(|property| property.value().trim().to_owned());
    let end_raw = end_property.map(|property| property.value().trim().to_owned());
    let start_line = start_property.map(property_line);
    let end_line = end_property.map(property_line);
    let start = start_raw.as_deref().and_then(normalize_calendar_time);
    let end = end_raw.as_deref().and_then(normalize_calendar_time);
    let sequence = event.property_value("SEQUENCE").and_then(|value| value.trim().parse::<u32>().ok()).unwrap_or(0);

    let recurrence =
        ["RRULE", "RDATE", "EXDATE"].iter().any(|key| event.properties().contains_key(*key) || event.multi_properties().contains_key(*key));
    if recurrence {
        warnings.push(InviteWarning::RecurrenceUnsupported);
    }

    let uid = event.property_value("UID").map(str::trim).unwrap_or_default().to_owned();
    let invalid_reason = if uid.is_empty() {
        Some("L'evento non dichiara un UID.".to_owned())
    } else if summary.is_empty() {
        Some("L'evento non dichiara un titolo.".to_owned())
    } else if start.is_none() {
        Some("L'evento non ha una data di inizio leggibile.".to_owned())
    } else {
        None
    };
    let valid = invalid_reason.is_none();
    let can_reply = method == "REQUEST" && valid && organizer.is_some() && !cancelled;

    Ok(CalendarInvite {
        method,
        uid,
        summary,
        summary_full,
        organizer,
        attendees,
        start,
        end,
        start_raw,
        end_raw,
        start_line,
        end_line,
        sequence,
        location: if location.is_empty() { None } else { Some(location) },
        recurrence,
        timezone,
        valid,
        invalid_reason,
        cancelled,
        can_reply,
        warnings,
    })
}

fn clean_field(raw: &str) -> String {
    raw.trim().replace(['\r', '\n'], " ")
}

/// Rebuild one property line with its original parameters (`DTSTART;TZID=Europe/Rome:...`,
/// `DTSTART;VALUE=DATE:...`). A hostile parameter pushes the line over the bound and the parameters
/// are dropped rather than copied into the reply.
fn property_line(property: &icalendar::Property) -> String {
    let value = escape_property_value(property.value().trim());
    let key = property.key().to_ascii_uppercase();
    let mut line = key.clone();
    for parameter in property.params().values() {
        line.push(';');
        line.push_str(&parameter.key().to_ascii_uppercase());
        line.push('=');
        let raw = parameter.value();
        if raw.contains([':', ',', ';']) {
            line.push('"');
            line.push_str(&raw.replace('"', ""));
            line.push('"');
        } else {
            line.push_str(raw);
        }
    }
    if line.chars().count() + 1 + value.chars().count() > MAX_INVITE_LINE_CHARS {
        return format!("{key}:{value}");
    }
    line.push(':');
    line.push_str(&value);
    line
}

fn bounded_field(raw: &str) -> (String, bool) {
    let clean = raw.trim().replace(['\r', '\n'], " ");
    let mut chars = clean.chars();
    let head = chars.by_ref().take(MAX_INVITE_FIELD_CHARS).collect::<String>();
    if chars.next().is_none() {
        return (head.trim_end().to_owned(), false);
    }
    let mut shortened = head.chars().take(MAX_INVITE_FIELD_CHARS - 1).collect::<String>();
    shortened.push('…');
    (shortened, true)
}

fn email_from_calendar_value(raw: &str) -> (Option<String>, bool) {
    let value = raw.trim();
    let email = value.strip_prefix("mailto:").or_else(|| value.strip_prefix("MAILTO:")).unwrap_or(value).trim().to_owned();
    let too_long = !email.is_empty() && email.chars().count() > MAX_INVITE_FIELD_CHARS;
    (valid_email(&email), too_long)
}

fn valid_email(email: &str) -> Option<String> {
    if email.is_empty() || email.chars().count() > MAX_INVITE_FIELD_CHARS || !email.contains('@') {
        return None;
    }
    if email
        .chars()
        .any(|character| character.is_control() || character.is_whitespace() || matches!(character, '<' | '>' | ',' | ';' | ':' | '"' | '\\'))
    {
        return None;
    }
    Some(email.to_owned())
}

fn attendees_of(event: &icalendar::Event) -> (Vec<String>, bool, bool) {
    let mut seen = std::collections::BTreeSet::new();
    let mut attendees = Vec::new();
    let mut truncated = false;
    let mut too_long = false;
    if let Some(properties) = event.multi_properties().get("ATTENDEE") {
        for property in properties {
            if attendees.len() >= MAX_INVITE_ATTENDEES {
                truncated = true;
                break;
            }
            let (email, long) = email_from_calendar_value(property.value());
            too_long |= long;
            if let Some(email) = email
                && seen.insert(email.to_ascii_lowercase())
            {
                attendees.push(email);
            }
        }
    }
    (attendees, truncated, too_long)
}

fn normalize_calendar_time(raw: &str) -> Option<String> {
    let value = raw.trim();
    if let Ok(date) = NaiveDateTime::parse_from_str(value, "%Y%m%dT%H%M%SZ") {
        return Some(date.format("%Y-%m-%dT%H:%M:%SZ").to_string());
    }
    if let Ok(date) = NaiveDateTime::parse_from_str(value, "%Y%m%dT%H%M%S") {
        return Some(date.format("%Y-%m-%dT%H:%M:%S").to_string());
    }
    if let Ok(date) = NaiveDate::parse_from_str(value, "%Y%m%d") {
        return Some(date.format("%Y-%m-%d").to_string());
    }
    None
}

/// Build the small `METHOD:REPLY` calendar attached to the draft. `partstat` is one of
/// `ACCEPTED`, `TENTATIVE` or `DECLINED`. Every value is escaped; no line break from the invite can
/// reach a property.
pub fn reply_calendar(invite: &CalendarInvite, attendee: &str, partstat: &str, now: DateTime<Utc>) -> String {
    let mut lines = vec![
        "BEGIN:VCALENDAR".to_owned(),
        "VERSION:2.0".to_owned(),
        "PRODID:-//Postino//Invite reply//IT".to_owned(),
        "METHOD:REPLY".to_owned(),
        "BEGIN:VEVENT".to_owned(),
        format!("UID:{}", escape_property_value(&invite.uid)),
        format!("DTSTAMP:{}", now.format("%Y%m%dT%H%M%SZ")),
        format!("SUMMARY:{}", escape_property_value(&invite.summary_full)),
        format!("SEQUENCE:{}", invite.sequence),
    ];
    if let Some(organizer) = invite.organizer.as_deref() {
        lines.push(format!("ORGANIZER;CN={}:mailto:{}", escape_parameter_value(organizer), escape_property_value(organizer)));
    }
    lines.push(format!(
        "ATTENDEE;PARTSTAT={};CN={}:mailto:{}",
        safe_partstat(partstat),
        escape_parameter_value(attendee),
        escape_property_value(attendee)
    ));
    if let Some(line) = invite.start_line.as_deref() {
        lines.push(line.to_owned());
    } else if let Some(raw) = invite.start_raw.as_deref().filter(|raw| !raw.is_empty()) {
        lines.push(format!("DTSTART:{}", escape_property_value(raw)));
    }
    if let Some(line) = invite.end_line.as_deref() {
        lines.push(line.to_owned());
    } else if let Some(raw) = invite.end_raw.as_deref().filter(|raw| !raw.is_empty()) {
        lines.push(format!("DTEND:{}", escape_property_value(raw)));
    }
    lines.push("END:VEVENT".to_owned());
    lines.push("END:VCALENDAR".to_owned());
    lines.into_iter().map(|line| fold_line(&line)).collect::<Vec<_>>().join("\r\n") + "\r\n"
}

fn safe_partstat(partstat: &str) -> &'static str {
    match partstat {
        "TENTATIVE" => "TENTATIVE",
        "DECLINED" => "DECLINED",
        _ => "ACCEPTED",
    }
}

fn escape_property_value(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut characters = raw.chars().peekable();
    while let Some(character) = characters.next() {
        match character {
            '\\' => out.push_str("\\\\"),
            ';' => out.push_str("\\;"),
            ',' => out.push_str("\\,"),
            '\r' => {
                if characters.peek() == Some(&'\n') {
                    characters.next();
                }
                out.push_str("\\n");
            }
            '\n' => out.push_str("\\n"),
            character if character.is_control() => {}
            character => out.push(character),
        }
    }
    out
}

fn escape_parameter_value(raw: &str) -> String {
    let escaped = escape_property_value(raw);
    // A parameter value cannot contain `:` or `,` unquoted; the simple quoted form keeps it opaque.
    if escaped.contains([':', ',', ';']) { format!("\"{}\"", escaped.replace('"', "")) } else { escaped }
}

fn fold_line(line: &str) -> String {
    const LIMIT: usize = 75;
    if line.len() <= LIMIT {
        return line.to_owned();
    }
    let mut out = String::new();
    let mut current = String::new();
    for character in line.chars() {
        if current.len() + character.len_utf8() > LIMIT {
            if out.is_empty() {
                out.push_str(&current);
            } else {
                out.push_str("\r\n ");
                out.push_str(&current);
            }
            current.clear();
        }
        current.push(character);
    }
    if !out.is_empty() {
        out.push_str("\r\n ");
    }
    out.push_str(&current);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> String {
        concat!(
            "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//Example//EN\r\nMETHOD:REQUEST\r\n",
            "BEGIN:VEVENT\r\nUID:invite-1@example.com\r\nDTSTAMP:20261020T090000Z\r\n",
            "DTSTART:20261105T090000Z\r\nDTEND:20261105T100000Z\r\n",
            "SUMMARY:Revisione preventivo\r\nLOCATION:Studio Postino\r\n",
            "ORGANIZER:mailto:giulia@example.com\r\n",
            "ATTENDEE;PARTSTAT=NEEDS-ACTION:mailto:federico@example.com\r\n",
            "ATTENDEE;PARTSTAT=ACCEPTED:mailto:marco@example.com\r\n",
            "END:VEVENT\r\nEND:VCALENDAR\r\n",
        )
        .to_owned()
    }

    #[test]
    fn a_request_is_parsed_with_its_organizer_and_start() {
        let invite = parse_invite(request().as_bytes()).expect("invite");
        assert_eq!(invite.method, "REQUEST");
        assert_eq!(invite.summary, "Revisione preventivo");
        assert_eq!(invite.organizer.as_deref(), Some("giulia@example.com"));
        assert_eq!(invite.start.as_deref(), Some("2026-11-05T09:00:00Z"));
        assert_eq!(invite.attendees.len(), 2);
        assert!(invite.valid && invite.can_reply && !invite.cancelled);
        assert!(invite.warnings.is_empty());
    }

    #[test]
    fn cancel_and_reply_change_the_card_state_but_never_offer_a_reply() {
        let cancel = parse_invite(request().replace("METHOD:REQUEST", "METHOD:CANCEL").as_bytes()).expect("cancel");
        assert!(cancel.cancelled && !cancel.can_reply);
        let reply = parse_invite(request().replace("METHOD:REQUEST", "METHOD:REPLY").as_bytes()).expect("reply");
        assert_eq!(reply.method, "REPLY");
        assert!(!reply.can_reply);
        let status = parse_invite(request().replace("DTSTAMP:", "STATUS:CANCELLED\r\nDTSTAMP:").as_bytes()).expect("status");
        assert!(status.cancelled && !status.can_reply);
    }

    #[test]
    fn recurrence_and_timezone_are_warned_not_expanded() {
        let recurrence = parse_invite(request().replace("SUMMARY:", "RRULE:FREQ=WEEKLY\r\nSUMMARY:").as_bytes()).expect("recurrence");
        assert!(recurrence.recurrence);
        assert!(recurrence.warnings.contains(&InviteWarning::RecurrenceUnsupported));
        let timezone =
            parse_invite(request().replace("DTSTART:20261105T090000Z", "DTSTART;TZID=Europe/Rome:20261105T090000").as_bytes()).expect("timezone");
        assert_eq!(timezone.timezone.as_deref(), Some("Europe/Rome"));
        assert!(timezone.warnings.contains(&InviteWarning::TimezoneUnsupported));
        assert_eq!(timezone.start.as_deref(), Some("2026-11-05T09:00:00"));
    }

    #[test]
    fn missing_fields_make_the_event_invalid_and_disable_reply() {
        let no_uid = parse_invite(request().replace("UID:invite-1@example.com\r\n", "").as_bytes()).expect("no uid");
        assert!(!no_uid.valid && !no_uid.can_reply && no_uid.invalid_reason.is_some());
        let no_start = parse_invite(request().replace("DTSTART:20261105T090000Z\r\n", "").as_bytes()).expect("no start");
        assert!(!no_start.valid && !no_start.can_reply);
        let no_organizer = parse_invite(request().replace("ORGANIZER:mailto:giulia@example.com\r\n", "").as_bytes()).expect("no organizer");
        assert!(no_organizer.valid, "the event itself is readable");
        assert!(!no_organizer.can_reply, "without an organizer there is nobody to reply to");
    }

    #[test]
    fn hostile_and_malformed_input_is_refused_or_bounded() {
        assert_eq!(parse_invite(b"not a calendar"), Err(InviteError::Malformed));
        assert_eq!(parse_invite(&[0xff, 0xfe, 0x00]), Err(InviteError::NotUtf8));
        assert_eq!(parse_invite(&vec![b'x'; MAX_CALENDAR_BYTES + 1]), Err(InviteError::TooLarge));
        assert_eq!(parse_invite(b"BEGIN:VCALENDAR\r\nEND:VCALENDAR\r\n"), Err(InviteError::NoEvent));
        let many = format!("{}BEGIN:VEVENT\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n", "BEGIN:VCALENDAR\r\n");
        let many = format!("{}{}", "BEGIN:X\r\n".repeat(MAX_CALENDAR_COMPONENTS + 1), many);
        assert_eq!(parse_invite(many.as_bytes()), Err(InviteError::TooManyComponents));
        for input in [b"BEGIN:VCALENDAR".as_slice(), b"\0\0\0", b"BEGIN:VEVENT\r\nEND:VCALENDAR"] {
            assert!(parse_invite(input).is_err(), "a malformed part must not produce a card");
        }
    }

    #[test]
    fn attendees_and_fields_are_capped() {
        let mut body = request().replace("SUMMARY:Revisione preventivo", &format!("SUMMARY:{}", "A".repeat(200)));
        for index in 0..(MAX_INVITE_ATTENDEES + 5) {
            body = body.replace("END:VEVENT", &format!("ATTENDEE:mailto:person{index}@example.com\r\nEND:VEVENT"));
        }
        let invite = parse_invite(body.as_bytes()).expect("capped invite");
        assert_eq!(invite.attendees.len(), MAX_INVITE_ATTENDEES);
        assert!(invite.warnings.contains(&InviteWarning::AttendeesTruncated));
        assert!(invite.warnings.contains(&InviteWarning::FieldsTruncated));
        assert!(invite.summary.chars().count() <= MAX_INVITE_FIELD_CHARS);
        assert!(invite.summary.ends_with('…'));
    }

    #[test]
    fn the_reply_calendar_is_escaped_and_never_leaks_a_header() {
        let mut invite = parse_invite(request().as_bytes()).expect("invite");
        invite.summary = "Cena\r\nBcc: attacker@example.com".to_owned();
        invite.summary_full = "Cena\r\nBcc: attacker@example.com".to_owned();
        let reply = reply_calendar(&invite, "federico@example.com", "ACCEPTED", DateTime::from_timestamp(0, 0).expect("epoch"));
        let unfolded = reply.replace("\r\n ", "");
        assert!(unfolded.contains("METHOD:REPLY"));
        assert!(unfolded.contains("PARTSTAT=ACCEPTED"));
        assert!(!unfolded.lines().any(|line| line.to_ascii_uppercase().starts_with("BCC:")));
        assert!(unfolded.contains("SUMMARY:Cena\\nBcc: attacker@example.com"));
        assert!(reply.ends_with("\r\n"));
    }

    #[test]
    fn the_reply_round_trips_tzid_sequence_and_the_full_summary() {
        let full = "Revisione preventiva con il team di prodotto e i fornitori".to_owned();
        let source = request()
            .replace("DTSTART:20261105T090000Z", "DTSTART;TZID=Europe/Rome:20261105T090000")
            .replace("DTEND:20261105T100000Z", "DTEND;TZID=Europe/Rome:20261105T100000")
            .replace("SUMMARY:Revisione preventivo", &format!("SUMMARY:{full}"))
            .replace("UID:invite-1@example.com\r\n", "UID:invite-1@example.com\r\nSEQUENCE:7\r\n");
        let invite = parse_invite(source.as_bytes()).expect("tzid invite");
        assert_eq!(invite.sequence, 7);
        assert_eq!(invite.summary_full, full);
        assert!(invite.summary.ends_with('…'), "the card still shows a capped title");
        let reply = reply_calendar(&invite, "federico@example.com", "TENTATIVE", DateTime::from_timestamp(0, 0).expect("epoch"));
        let parsed = parse_invite(reply.as_bytes()).expect("the reply parses back");
        assert_eq!(parsed.method, "REPLY");
        assert_eq!(parsed.start_line.as_deref(), Some("DTSTART;TZID=Europe/Rome:20261105T090000"));
        assert_eq!(parsed.end_line.as_deref(), Some("DTEND;TZID=Europe/Rome:20261105T100000"));
        assert_eq!(parsed.sequence, 7);
        assert_eq!(parsed.summary_full, full, "the reply carries the whole title, not the 64-char card text");
        assert_eq!(parsed.start.as_deref(), Some("2026-11-05T09:00:00"));
    }

    #[test]
    fn an_all_day_value_date_reply_keeps_the_value_parameter() {
        let source = request()
            .replace("DTSTART:20261105T090000Z", "DTSTART;VALUE=DATE:20261105")
            .replace("DTEND:20261105T100000Z", "DTEND;VALUE=DATE:20261106");
        let invite = parse_invite(source.as_bytes()).expect("all-day invite");
        let reply = reply_calendar(&invite, "federico@example.com", "ACCEPTED", DateTime::from_timestamp(0, 0).expect("epoch"));
        let parsed = parse_invite(reply.as_bytes()).expect("the reply parses back");
        assert_eq!(parsed.start_line.as_deref(), Some("DTSTART;VALUE=DATE:20261105"));
        assert_eq!(parsed.end_line.as_deref(), Some("DTEND;VALUE=DATE:20261106"));
        assert_eq!(parsed.start.as_deref(), Some("2026-11-05"));
    }

    #[test]
    fn addresses_and_begin_counting_are_capped_at_sixty_four_characters() {
        let local = "a".repeat(MAX_INVITE_FIELD_CHARS - "@example.com".len());
        let at_cap = format!("{local}@example.com");
        assert_eq!(at_cap.chars().count(), MAX_INVITE_FIELD_CHARS);
        let accepted = parse_invite(request().replace("giulia@example.com", &at_cap).as_bytes()).expect("64-char organizer");
        assert_eq!(accepted.organizer.as_deref(), Some(at_cap.as_str()));
        let over = format!("{local}x@example.com");
        let refused = parse_invite(request().replace("giulia@example.com", &over).as_bytes()).expect("over-long organizer");
        assert_eq!(refused.organizer, None);
        assert!(refused.warnings.contains(&InviteWarning::FieldsTruncated));

        let lower = request().replace("BEGIN:VEVENT", "bEgIn:VeVeNt");
        assert!(parse_invite(lower.as_bytes()).is_ok(), "component parsing must ignore BEGIN case");
        let mixed = format!("{}{}", "BeGiN:X\r\n".repeat(MAX_CALENDAR_COMPONENTS + 1), request());
        assert_eq!(parse_invite(mixed.as_bytes()), Err(InviteError::TooManyComponents));
    }
}
