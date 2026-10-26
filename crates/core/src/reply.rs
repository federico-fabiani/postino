//! Deterministic compose prefill for reply, reply-all and forward.
//!
//! This module is pure: it reads an [`Account`], a parent [`Message`] and the parent's `Reply-To`
//! list, and returns the recipients, subject, body and reply linkage a composer should start from.
//! The desktop app renders it, the tests pin its golden strings, and no frontend keeps a second
//! copy of the recipient rules.
//!
//! The quote is deliberately plain text and UTC: two runs on two machines produce the same bytes,
//! and a reply never contains HTML. Forwarded attachment metadata is copied here; the bytes are
//! copied by the engine, which owns the encrypted blob store.

use std::collections::BTreeSet;

use chrono::{DateTime, Utc};

use crate::model::{Account, Address, Attachment, Message};

/// What the composer is preparing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplyMode {
    Reply,
    ReplyAll,
    Forward,
}

/// The starting state for a reply/forward composer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplyPrefill {
    pub to: Vec<Address>,
    pub cc: Vec<Address>,
    pub subject: String,
    pub body: String,
    /// Parent `Message-ID`, without angle brackets. Set for reply and reply-all, never for forward.
    pub in_reply_to: Option<String>,
    /// Parent attachments to copy. The ids still name the parent rows: the composer treats them as
    /// sources and the engine gives the copies new ids and the same bytes.
    pub attachments: Vec<Attachment>,
}

/// The signature block, or an empty string when the account has no signature.
///
/// `-- ` is the conventional signature separator and the short line the brief asks for before the
/// quoted body. Whitespace-only input is no signature at all.
#[must_use]
pub fn signature_block(signature: &str) -> String {
    let signature = signature.trim_end_matches(['\r', '\n']);
    if signature.trim().is_empty() {
        return String::new();
    }
    format!("-- \n{signature}")
}

/// A new message body: the signature followed by one blank line for the reply text.
#[must_use]
pub fn new_message_body(signature: &str) -> String {
    let block = signature_block(signature);
    if block.is_empty() { String::new() } else { format!("{block}\n\n") }
}

/// Prefix every line with `> `. An empty line becomes a bare `>` so the quote stays readable.
#[must_use]
pub fn quote_body(body: &str) -> String {
    body.replace("\r\n", "\n").lines().map(|line| if line.is_empty() { ">".to_owned() } else { format!("> {line}") }).collect::<Vec<_>>().join("\n")
}

/// The deterministic reply quote header and body.
#[must_use]
pub fn reply_quote(message: &Message) -> String {
    format!("Il giorno {} {} ha scritto:\n{}", utc_stamp(message.summary.date), message.summary.from.display(), quote_body(&message.body_text))
}

/// The deterministic forward block. Bcc is never copied: it is not in the model of a visible
/// forward, and a forwarded Bcc would leak the original blind recipients.
#[must_use]
pub fn forward_block(message: &Message) -> String {
    let mut block = format!(
        "---------- Messaggio inoltrato ----------\nDa: {}\nData: {}\nA: {}\n",
        message.summary.from.display(),
        utc_stamp(message.summary.date),
        message.summary.to.iter().map(Address::display).collect::<Vec<_>>().join(", ")
    );
    if !message.cc.is_empty() {
        block.push_str(&format!("Cc: {}\n", message.cc.iter().map(Address::display).collect::<Vec<_>>().join(", ")));
    }
    block.push_str(&format!("Oggetto: {}\n\n", message.summary.subject));
    block.push_str(&message.body_text.replace("\r\n", "\n"));
    block
}

/// A reply subject, with `Re:` present exactly once.
#[must_use]
pub fn reply_subject(subject: &str) -> String {
    add_prefix(subject, &["re:"], "Re:")
}

/// A forward subject, with `Fwd:` present exactly once.
#[must_use]
pub fn forward_subject(subject: &str) -> String {
    add_prefix(subject, &["fwd:", "fw:"], "Fwd:")
}

/// Build the composer state for one reply or forward.
#[must_use]
pub fn compose_reply(account: &Account, message: &Message, parent_message_id: Option<&str>, reply_to: &[Address], mode: ReplyMode) -> ReplyPrefill {
    let own = account.email.trim().to_ascii_lowercase();
    let (to, cc) = match mode {
        ReplyMode::Forward => (Vec::new(), Vec::new()),
        ReplyMode::Reply => (reply_targets(message, reply_to, &own), Vec::new()),
        ReplyMode::ReplyAll => {
            let to = reply_targets(message, reply_to, &own);
            let taken = to.iter().map(|address| address.email.trim().to_ascii_lowercase()).collect::<BTreeSet<_>>();
            let mut cc = unique_recipients(message.summary.to.iter().chain(message.cc.iter()).cloned(), &own, &taken);
            // A recipient already in To must not repeat in Cc.
            cc.retain(|address| !taken.contains(&address.email.trim().to_ascii_lowercase()));
            (to, cc)
        }
    };
    let signature = signature_block(&account.signature);
    let (subject, body, in_reply_to, attachments) = match mode {
        ReplyMode::Forward => {
            let quoted = forward_block(message);
            let body = if signature.is_empty() { quoted } else { format!("{signature}\n\n{quoted}") };
            (forward_subject(&message.summary.subject), body, None, message.attachments.clone())
        }
        ReplyMode::Reply | ReplyMode::ReplyAll => {
            let quoted = reply_quote(message);
            // Replying to a message that already carries this signature (a Sent copy) must not
            // insert the same block a second time above the quote.
            let body = if signature.is_empty() || already_signed(&message.body_text, &account.signature) {
                quoted
            } else {
                format!("{signature}\n\n{quoted}")
            };
            let subject = reply_subject(&message.summary.subject);
            (subject, body, parent_message_id.map(str::to_owned), Vec::new())
        }
    };
    ReplyPrefill { to, cc, subject, body, in_reply_to, attachments }
}

/// The visible `To` for a reply: `Reply-To` when present, otherwise `From`. The account's own
/// addresses are removed. When the parent is our own Sent copy, the original `To` is the honest
/// fallback instead of an empty reply.
fn reply_targets(message: &Message, reply_to: &[Address], own: &str) -> Vec<Address> {
    let source = if reply_to.is_empty() { vec![message.summary.from.clone()] } else { reply_to.to_vec() };
    let targets = unique_recipients(source, own, &BTreeSet::new());
    if targets.is_empty() {
        // The parent is our own Sent copy, or its Reply-To is entirely our own address. Fall back
        // to the visible original recipients instead of opening an empty reply.
        return unique_recipients(message.summary.to.iter().cloned(), own, &BTreeSet::new());
    }
    targets
}

fn unique_recipients(addresses: impl IntoIterator<Item = Address>, own: &str, taken: &BTreeSet<String>) -> Vec<Address> {
    let mut seen = taken.clone();
    let mut unique = Vec::new();
    for address in addresses {
        let email = address.email.trim().to_ascii_lowercase();
        if email.is_empty() || email == own || !seen.insert(email) {
            continue;
        }
        unique.push(Address::new(address.name.trim(), address.email.trim()));
    }
    unique
}

fn already_signed(body: &str, signature: &str) -> bool {
    let signature = signature.trim();
    if signature.is_empty() {
        return false;
    }
    let wanted = signature.lines().map(str::trim_end).collect::<Vec<_>>();
    if wanted.is_empty() {
        return false;
    }
    let lines = body.replace("\r\n", "\n");
    let lines = lines.lines().map(str::trim_end).collect::<Vec<_>>();
    // The signature block starts with the conventional `-- ` line. Matching only that block avoids
    // suppressing a new signature merely because the parent body mentions the same name.
    for (index, line) in lines.iter().enumerate() {
        if *line != "--" && *line != "-- " {
            continue;
        }
        let start = index.saturating_add(1);
        let end = start.saturating_add(wanted.len());
        if end <= lines.len() && lines.get(start..end) == Some(wanted.as_slice()) {
            return true;
        }
    }
    false
}

fn add_prefix(subject: &str, prefixes: &[&str], prefix: &str) -> String {
    let subject = subject.trim();
    let lowercase = subject.to_ascii_lowercase();
    if prefixes.iter().any(|candidate| lowercase.starts_with(candidate)) { subject.to_owned() } else { format!("{prefix} {subject}") }
}

/// UTC, minute precision: a quote is a record of the parent's own date, not of the moment the
/// composer opened. The fixed `UTC` suffix prevents a renderer from guessing a local zone.
fn utc_stamp(now: DateTime<Utc>) -> String {
    format!("{} UTC", now.format("%d/%m/%Y %H:%M"))
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;
    use crate::model::MessageSummary;

    fn account(signature: &str) -> Account {
        Account {
            id: "acc_test".to_owned(),
            name: "Federico".to_owned(),
            email: "federico@example.it".to_owned(),
            provider: "custom".to_owned(),
            signature: signature.to_owned(),
        }
    }

    fn message(from: (&str, &str), to: Vec<(&str, &str)>, cc: Vec<(&str, &str)>, subject: &str, body: &str) -> Message {
        let date = Utc.with_ymd_and_hms(2026, 10, 25, 14, 30, 0).single().expect("a valid instant");
        Message {
            summary: MessageSummary {
                id: "msg_parent".to_owned(),
                thread_id: "thr_parent".to_owned(),
                folder_id: "inbox".to_owned(),
                from: Address::new(from.0, from.1),
                to: to.into_iter().map(|(name, email)| Address::new(name, email)).collect(),
                subject: subject.to_owned(),
                snippet: String::new(),
                date,
                unread: false,
                starred: false,
                has_attachments: false,
                labels: Vec::new(),
            },
            cc: cc.into_iter().map(|(name, email)| Address::new(name, email)).collect(),
            bcc: Vec::new(),
            body_text: body.to_owned(),
            body_html: None,
            remote_images: 0,
            attachments: Vec::new(),
        }
    }

    #[test]
    fn a_reply_uses_the_sender_and_prefixes_the_subject_once() {
        let parent = message(("Giulia", "giulia@example.com"), vec![("Federico", "federico@example.it")], Vec::new(), "Preventivo", "Ciao");
        let prefill = compose_reply(&account("Federico\nfederico@example.it"), &parent, Some("parent@example.com"), &[], ReplyMode::Reply);
        assert_eq!(prefill.to, vec![Address::new("Giulia", "giulia@example.com")]);
        assert!(prefill.cc.is_empty());
        assert_eq!(prefill.subject, "Re: Preventivo");
        assert_eq!(prefill.in_reply_to.as_deref(), Some("parent@example.com"));
        assert!(prefill.attachments.is_empty());
        assert_eq!(
            prefill.body,
            "-- \nFederico\nfederico@example.it\n\nIl giorno 25/10/2026 14:30 UTC Giulia <giulia@example.com> ha scritto:\n> Ciao"
        );

        let already = message(("Giulia", "giulia@example.com"), Vec::new(), Vec::new(), "Re: Preventivo", "Ciao");
        assert_eq!(compose_reply(&account(""), &already, Some("p@example.com"), &[], ReplyMode::Reply).subject, "Re: Preventivo");
        assert_eq!(compose_reply(&account(""), &already, Some("p@example.com"), &[], ReplyMode::Forward).subject, "Fwd: Re: Preventivo");
        for subject in ["Fwd: Preventivo", "Fw: Preventivo", "FWD: Preventivo"] {
            let parent = message(("Giulia", "giulia@example.com"), Vec::new(), Vec::new(), subject, "Ciao");
            assert_eq!(compose_reply(&account(""), &parent, None, &[], ReplyMode::Forward).subject, subject, "the forward prefix was doubled");
        }
    }

    #[test]
    fn reply_all_uses_reply_to_and_never_a_bcc() {
        let mut parent = message(
            ("Giulia", "giulia@example.com"),
            vec![("Federico", "federico@example.it"), ("Marco", "marco@example.com"), ("Giulia", "GIULIA@example.com")],
            vec![("Studio", "studio@example.com"), ("Marco", "marco@example.com")],
            "Pranzo",
            "Ciao",
        );
        parent.bcc = vec![Address::new("Segreto", "bcc@example.com")];
        let prefill = compose_reply(
            &account(""),
            &parent,
            Some("parent@example.com"),
            &[Address::new("Segreteria", "segreteria@example.com")],
            ReplyMode::ReplyAll,
        );
        assert_eq!(prefill.to, vec![Address::new("Segreteria", "segreteria@example.com")]);
        assert_eq!(
            prefill.cc,
            vec![
                Address::new("Marco", "marco@example.com"),
                Address::new("Giulia", "GIULIA@example.com"),
                Address::new("Studio", "studio@example.com")
            ],
        );
        assert!(!prefill.body.contains("bcc@example.com"), "the Bcc leaked into the forwarded/replied body");
    }

    #[test]
    fn replying_to_our_own_sent_copy_targets_the_original_recipients() {
        let parent = message(("Federico", "federico@example.it"), vec![("Giulia", "giulia@example.com")], Vec::new(), "Ciao", "Corpo");
        let prefill = compose_reply(&account(""), &parent, Some("sent@example.com"), &[], ReplyMode::Reply);
        assert_eq!(prefill.to, vec![Address::new("Giulia", "giulia@example.com")]);
    }

    #[test]
    fn a_reply_to_list_containing_only_our_address_falls_back_to_the_visible_recipients() {
        let parent = message(("Giulia", "giulia@example.com"), vec![("Federico", "federico@example.it")], Vec::new(), "Ciao", "Corpo");
        let prefill =
            compose_reply(&account(""), &parent, Some("parent@example.com"), &[Address::new("Federico", "federico@example.it")], ReplyMode::Reply);
        assert!(prefill.to.is_empty(), "the only Reply-To address is our own; there is no visible non-self recipient");
        let parent = message(
            ("Giulia", "giulia@example.com"),
            vec![("Federico", "federico@example.it"), ("Marco", "marco@example.com")],
            Vec::new(),
            "Ciao",
            "Corpo",
        );
        let prefill =
            compose_reply(&account(""), &parent, Some("parent@example.com"), &[Address::new("Federico", "federico@example.it")], ReplyMode::Reply);
        assert_eq!(prefill.to, vec![Address::new("Marco", "marco@example.com")]);
    }

    #[test]
    fn a_reply_to_our_sent_copy_does_not_repeat_the_signature() {
        let signature = "Federico\nfederico@example.it";
        let parent = message(
            ("Federico", "federico@example.it"),
            vec![("Giulia", "giulia@example.com")],
            Vec::new(),
            "Ciao",
            "Corpo\n\n-- \nFederico\nfederico@example.it",
        );
        let prefill = compose_reply(&account(signature), &parent, Some("sent@example.com"), &[], ReplyMode::Reply);
        assert_eq!(prefill.body.matches("-- \n").count(), 1, "the signature was inserted twice: {}", prefill.body);
        assert!(prefill.body.starts_with("Il giorno "), "no new signature block before the quote: {}", prefill.body);
    }

    #[test]
    fn a_plain_mention_of_the_signature_name_is_not_a_duplicate() {
        let signature = "Federico";
        let parent = message(
            ("Giulia", "giulia@example.com"),
            vec![("Federico", "federico@example.it")],
            Vec::new(),
            "Ciao",
            "Federico mi ha detto di scriverti: ci vediamo lunedì.",
        );
        let prefill = compose_reply(&account(signature), &parent, Some("parent@example.com"), &[], ReplyMode::Reply);
        assert!(prefill.body.starts_with("-- \nFederico\n\nIl giorno "), "the signature was wrongly suppressed: {}", prefill.body);
    }

    #[test]
    fn the_quote_and_forward_headers_are_golden_and_deterministic() {
        let mut parent = message(
            ("Giulia", "giulia@example.com"),
            vec![("Federico", "federico@example.it")],
            vec![("Marco", "marco@example.com")],
            "Preventivo",
            "Riga uno\n\nRiga tre",
        );
        parent.attachments =
            vec![Attachment { id: "att_1".to_owned(), filename: "preventivo.pdf".to_owned(), mime: "application/pdf".to_owned(), size: 3 }];
        let forward = compose_reply(&account(""), &parent, None, &[], ReplyMode::Forward);
        assert!(forward.to.is_empty());
        assert!(forward.cc.is_empty());
        assert_eq!(forward.subject, "Fwd: Preventivo");
        assert_eq!(forward.in_reply_to, None);
        assert_eq!(forward.attachments.len(), 1);
        assert_eq!(
            forward.body,
            "---------- Messaggio inoltrato ----------\nDa: Giulia <giulia@example.com>\nData: 25/10/2026 14:30 UTC\nA: Federico <federico@example.it>\nCc: Marco <marco@example.com>\nOggetto: Preventivo\n\nRiga uno\n\nRiga tre"
        );
    }

    #[test]
    fn a_new_message_body_is_the_signature_and_a_blank_line() {
        assert_eq!(new_message_body(""), "");
        assert_eq!(new_message_body("  \n"), "");
        assert_eq!(new_message_body("Federico\nfederico@example.it"), "-- \nFederico\nfederico@example.it\n\n");
    }

    #[test]
    fn quote_body_wraps_every_line_including_blanks() {
        assert_eq!(quote_body("Ciao\n\nGiulia"), "> Ciao\n>\n> Giulia");
        assert_eq!(quote_body(""), "");
    }
}
