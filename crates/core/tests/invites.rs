//! P12d: a received `text/calendar` part is read safely and an answer becomes a draft only.
//!
//! Every fixture is synthetic (`example.com` / `example.it`), lives in a throwaway encrypted store,
//! and nothing here opens a socket. The tests drive the same `ops::dispatch` the CLI and MCP use.

use base64::Engine as _;
use postino_core::{Engine, InviteResponse, ops};
use postino_store::{FolderKind, Store, StoreKey};
use serde_json::{Value, json};

const NOW: i64 = 1_700_000_000_000;
const REQUEST_UID: &str = "invito-1@example.com";

struct Fixture {
    dir: tempfile::TempDir,
    key: StoreKey,
}

impl Fixture {
    fn new() -> Self {
        let mut bytes = [0u8; 32];
        getrandom::fill(&mut bytes).expect("OS random source");
        Self { dir: tempfile::tempdir().expect("temp dir"), key: StoreKey::from_bytes(bytes) }
    }

    fn open(&self) -> Store {
        Store::open(&self.dir.path().join("mail.db"), &self.key).expect("open the store")
    }

    fn seed(&self) {
        let store = self.open();
        store
            .upsert_account(&postino_store::Account {
                id: "acc_invite".to_owned(),
                name: "Studio Postino".to_owned(),
                email: "federico@example.it".to_owned(),
                provider: "demo".to_owned(),
                signature: String::new(),
                created_at: NOW,
            })
            .expect("account");
        for (id, name, kind) in [
            ("inbox", "Posta in arrivo", FolderKind::Inbox),
            ("drafts", "Bozze", FolderKind::Drafts),
            ("trash", "Cestino", FolderKind::Trash),
            ("sent", "Posta inviata", FolderKind::Sent),
        ] {
            store
                .upsert_folder(&postino_store::Folder {
                    id: id.to_owned(),
                    account_id: "acc_invite".to_owned(),
                    name: name.to_owned(),
                    kind,
                    unread: 0,
                    total: 0,
                    uid_validity: None,
                    uid_next: None,
                    last_sync_at: None,
                })
                .expect("folder");
        }
        let fixtures = [
            ("msg_request", "Revisione preventivo", Some(request())),
            ("msg_cancel", "Revisione annullata", Some(cancel())),
            ("msg_reply", "Revisione: risposta", Some(reply())),
            ("msg_noorganizer", "Senza organizzatore", Some(no_organizer())),
            ("msg_broken", "Calendario rotto", Some(broken())),
            ("msg_big", "Calendario enorme", None),
        ];
        for (id, subject, calendar) in fixtures {
            store
                .upsert_message(&postino_store::Message {
                    summary: postino_store::MessageSummary {
                        id: id.to_owned(),
                        account_id: "acc_invite".to_owned(),
                        folder_id: "inbox".to_owned(),
                        thread_id: format!("thr_{id}"),
                        from: postino_store::Address::new("Giulia Ferri", "giulia.ferri@example.com"),
                        to: vec![postino_store::Address::new("Federico", "federico@example.it")],
                        subject: subject.to_owned(),
                        snippet: "Invito".to_owned(),
                        date: NOW,
                        unread: false,
                        starred: false,
                        has_attachments: true,
                        labels: Vec::new(),
                    },
                    uid: None,
                    message_id: Some(format!("{id}@example.com")),
                    cc: Vec::new(),
                    bcc: Vec::new(),
                    body_text: "Invito in allegato".to_owned(),
                    body_html: None,
                    remote_images: 0,
                    size: None,
                })
                .expect("message");
            let attachment_id = format!("att_{id}");
            let size = calendar.as_ref().map_or(262_145, |calendar| u64::try_from(calendar.len()).expect("size"));
            store
                .replace_attachments(
                    id,
                    &[postino_store::Attachment {
                        id: attachment_id.clone(),
                        filename: "invito.ics".to_owned(),
                        mime: "text/calendar".to_owned(),
                        size,
                        blob_hash: None,
                    }],
                )
                .expect("attachment row");
            if let Some(calendar) = calendar {
                let store = self.open();
                let size = u64::try_from(calendar.len()).expect("size");
                let hash = store.blobs().put(calendar.as_bytes()).expect("calendar blob");
                store.set_attachment_blob_hash(id, &attachment_id, Some(&hash), size).expect("calendar hash");
            }
        }

        let sent_calendar = request();
        seed_parts(&store, "msg_sent", "sent", "Invito inviato", &[("att_msg_sent", "invito.ics", "text/calendar", sent_calendar.len())]);
        put_calendar(&store, "msg_sent", "att_msg_sent", &sent_calendar);

        let fallback = request();
        seed_parts(
            &store,
            "msg_fallback",
            "inbox",
            "Invito senza tipo dichiarato",
            &[("att_msg_fallback", "Invito.ICS", "application/octet-stream", fallback.len())],
        );
        put_calendar(&store, "msg_fallback", "att_msg_fallback", &fallback);

        let with_unrelated = request();
        seed_parts(
            &store,
            "msg_unrelated",
            "inbox",
            "Invito con allegato estraneo",
            &[
                ("att_unrelated_big", "preventivo.pdf", "application/pdf", 10 * 1024 * 1024),
                ("att_unrelated_ics", "invito.ics", "text/calendar", with_unrelated.len()),
            ],
        );
        put_calendar(&store, "msg_unrelated", "att_unrelated_ics", &with_unrelated);

        let missing = request();
        seed_parts(&store, "msg_missing", "inbox", "Calendario mancante", &[("att_msg_missing", "invito.ics", "text/calendar", missing.len())]);

        let corrupt = request().replace("SUMMARY:Revisione preventivo", "SUMMARY:Calendario corrotto isolato");
        seed_parts(&store, "msg_corrupt", "inbox", "Calendario corrotto", &[("att_msg_corrupt", "invito.ics", "text/calendar", corrupt.len())]);
        let hash = put_calendar(&store, "msg_corrupt", "att_msg_corrupt", &corrupt);
        let path = store.blobs().root().join(&hash[..2]).join(format!("{hash}.blob"));
        std::fs::write(path, b"not a sealed blob").expect("corrupt blob");

        let tzid = request()
            .replace("DTSTART:20261105T090000Z", "DTSTART;TZID=Europe/Rome:20261105T090000")
            .replace("DTEND:20261105T100000Z", "DTEND;TZID=Europe/Rome:20261105T100000")
            .replace("SUMMARY:Revisione preventivo", &format!("SUMMARY:{}", "Pianificazione trimestrale con il team allargato e i fornitori esterni"))
            .replace("UID:invito-1@example.com\r\n", "UID:invito-1@example.com\r\nSEQUENCE:4\r\n");
        seed_parts(&store, "msg_tzid", "inbox", "Invito con fuso", &[("att_msg_tzid", "invito.ics", "text/calendar", tzid.len())]);
        put_calendar(&store, "msg_tzid", "att_msg_tzid", &tzid);

        let allday = request()
            .replace("DTSTART:20261105T090000Z", "DTSTART;VALUE=DATE:20261105")
            .replace("DTEND:20261105T100000Z", "DTEND;VALUE=DATE:20261106");
        seed_parts(&store, "msg_allday", "inbox", "Invito tutto il giorno", &[("att_msg_allday", "invito.ics", "text/calendar", allday.len())]);
        put_calendar(&store, "msg_allday", "att_msg_allday", &allday);

        let utf8 = request()
            .replace("SUMMARY:Revisione preventivo", "SUMMARY:Cena\\, tè\\; caffè — ünïcode")
            .replace("LOCATION:Studio Postino", "LOCATION:Sala\\, piano 1\\; online");
        seed_parts(&store, "msg_utf8", "inbox", "Invito accentato", &[("att_msg_utf8", "invito.ics", "text/calendar", utf8.len())]);
        put_calendar(&store, "msg_utf8", "att_msg_utf8", &utf8);
    }
}

fn seed_parts(store: &Store, id: &str, folder: &str, subject: &str, parts: &[(&str, &str, &str, usize)]) {
    store
        .upsert_message(&postino_store::Message {
            summary: postino_store::MessageSummary {
                id: id.to_owned(),
                account_id: "acc_invite".to_owned(),
                folder_id: folder.to_owned(),
                thread_id: format!("thr_{id}"),
                from: postino_store::Address::new("Giulia Ferri", "giulia.ferri@example.com"),
                to: vec![postino_store::Address::new("Federico", "federico@example.it")],
                subject: subject.to_owned(),
                snippet: "Invito".to_owned(),
                date: NOW,
                unread: false,
                starred: false,
                has_attachments: !parts.is_empty(),
                labels: Vec::new(),
            },
            uid: None,
            message_id: Some(format!("{id}@example.com")),
            cc: Vec::new(),
            bcc: Vec::new(),
            body_text: "Invito in allegato".to_owned(),
            body_html: None,
            remote_images: 0,
            size: None,
        })
        .expect("message");
    let rows = parts
        .iter()
        .map(|(attachment_id, filename, mime, size)| postino_store::Attachment {
            id: (*attachment_id).to_owned(),
            filename: (*filename).to_owned(),
            mime: (*mime).to_owned(),
            size: u64::try_from(*size).expect("size"),
            blob_hash: None,
        })
        .collect::<Vec<_>>();
    store.replace_attachments(id, &rows).expect("attachment rows");
}

fn put_calendar(store: &Store, message_id: &str, attachment_id: &str, calendar: &str) -> String {
    let hash = store.blobs().put(calendar.as_bytes()).expect("calendar blob");
    store.set_attachment_blob_hash(message_id, attachment_id, Some(&hash), u64::try_from(calendar.len()).expect("size")).expect("calendar hash");
    hash
}

fn call(engine: &mut Engine, name: &str, args: Value) -> Value {
    ops::dispatch(engine, name, args).unwrap_or_else(|error| panic!("{name}: {error}"))
}

fn ics(method: &str, organizer: bool, uid: &str) -> String {
    let organizer = if organizer { "ORGANIZER:mailto:giulia.ferri@example.com\r\n" } else { "" };
    format!(
        "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//Example//Inviti//IT\r\nMETHOD:{method}\r\nBEGIN:VEVENT\r\nUID:{uid}\r\nDTSTAMP:20261024T084200Z\r\nDTSTART:20261105T090000Z\r\nDTEND:20261105T100000Z\r\nSUMMARY:Revisione preventivo\r\nLOCATION:Studio Postino\r\n{organizer}ATTENDEE:mailto:marco@example.com\r\nATTENDEE:mailto:federico@example.it\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n"
    )
}

fn request() -> String {
    ics("REQUEST", true, REQUEST_UID)
}

fn cancel() -> String {
    ics("CANCEL", true, REQUEST_UID)
}

fn reply() -> String {
    ics("REPLY", true, REQUEST_UID)
}

fn no_organizer() -> String {
    ics("REQUEST", false, REQUEST_UID)
}

fn broken() -> String {
    "BEGIN:VCALENDAR\r\nMETHOD:REQUEST\r\nBEGIN:VEVENT\r\nSUMMARY:Senza fine\r\n".to_owned()
}

#[test]
fn a_request_is_read_and_the_reply_draft_goes_only_to_the_organizer() {
    let fixture = Fixture::new();
    fixture.seed();
    let mut engine = Engine::open(fixture.open()).expect("engine");

    let invite = call(&mut engine, "message_invite", json!({ "id": "msg_request" }));
    let invite = &invite["invite"];
    assert_eq!(invite["method"], json!("request"));
    assert_eq!(invite["summary"], json!("Revisione preventivo"));
    assert_eq!(invite["organizer"], json!("giulia.ferri@example.com"));
    assert_eq!(invite["start"], json!("2026-11-05T09:00:00Z"));
    assert_eq!(invite["end"], json!("2026-11-05T10:00:00Z"));
    assert_eq!(invite["location"], json!("Studio Postino"));
    assert_eq!(invite["valid"], json!(true));
    assert_eq!(invite["can_reply"], json!(true));
    assert_eq!(invite["attendees"].as_array().map(Vec::len), Some(2));
    assert_eq!(invite["warnings"], json!([]));

    let draft = call(&mut engine, "invite_reply_draft", json!({ "message": "msg_request", "response": "accept" }));
    let draft = &draft["draft"];
    let draft_id = draft["summary"]["id"].as_str().expect("draft id").to_owned();
    assert_eq!(draft["summary"]["folder_id"], json!("drafts"));
    assert_eq!(draft["summary"]["subject"], json!("Accettato: Revisione preventivo"));
    assert_eq!(draft["summary"]["to"].as_array().map(Vec::len), Some(1), "only the organizer");
    assert_eq!(draft["summary"]["to"][0]["email"], json!("giulia.ferri@example.com"));
    assert_eq!(draft["cc"], json!([]));
    assert_eq!(draft["bcc"], json!([]), "no Bcc leak");
    assert!(draft["body_text"].as_str().expect("body").contains("Nessuna risposta automatica"));
    assert_eq!(engine.draft_reply_link(&draft_id), Some("msg_request@example.com"));

    let dry_run = call(&mut engine, "send_dry_run", json!({ "draft": draft_id }));
    let raw = base64::engine::general_purpose::STANDARD.decode(dry_run["raw_base64"].as_str().expect("raw")).expect("base64");
    let raw = String::from_utf8(raw).expect("utf-8 submission");
    assert!(raw.contains("In-Reply-To: <msg_request@example.com>"), "exact parent Message-ID");
    assert!(raw.contains("References: <msg_request@example.com>"), "exact references chain");
    assert!(raw.contains("text/calendar"), "the reply calendar travels with the draft");

    assert_eq!(draft["attachments"].as_array().map(Vec::len), Some(1));
    assert_eq!(draft["attachments"][0]["mime"], json!("text/calendar"));
    let source = engine.attachment_bytes("att_msg_request").expect("the source calendar stays readable");
    assert!(String::from_utf8(source).expect("utf-8").contains("METHOD:REQUEST"));
    let reply_attachment = draft["attachments"][0]["id"].as_str().expect("attachment id").to_owned();
    let bytes = engine.attachment_bytes(&reply_attachment).expect("reply calendar bytes");
    let reply_calendar = String::from_utf8(bytes).expect("utf-8 calendar");
    assert!(reply_calendar.contains("METHOD:REPLY"));
    assert!(reply_calendar.contains("PARTSTAT=ACCEPTED"));
    assert!(reply_calendar.contains(REQUEST_UID));
    assert!(!reply_calendar.contains("marco@example.com"), "attendees are never copied");
    assert!(!reply_calendar.contains("Bcc:"));

    let outbox = engine.outbox_list(Some("acc_invite"), 10, 0).expect("outbox");
    assert!(outbox.entries.is_empty(), "drafting an answer never sends it");

    assert_eq!(engine.message(&draft_id).expect("draft").summary.folder_id, "drafts");
}

#[test]
fn a_reply_keeps_tzid_value_date_sequence_and_the_whole_summary() {
    let fixture = Fixture::new();
    fixture.seed();
    let mut engine = Engine::open(fixture.open()).expect("engine");

    let invite = call(&mut engine, "message_invite", json!({ "id": "msg_tzid" }))["invite"].clone();
    assert_eq!(invite["can_reply"], json!(true));
    assert_eq!(invite["timezone"], json!("Europe/Rome"));
    assert!(invite["summary"].as_str().expect("card summary").ends_with('…'), "the card stays capped");
    {
        let draft = call(&mut engine, "invite_reply_draft", json!({ "message": "msg_tzid", "response": "tentative" }));
        let attachment = draft["draft"]["attachments"][0]["id"].as_str().expect("attachment").to_owned();
        let bytes = engine.attachment_bytes(&attachment).expect("calendar bytes");
        let parsed = postino_sync::parse_invite(&bytes).expect("the reply calendar parses back");
        assert_eq!(parsed.method, "REPLY");
        assert_eq!(parsed.start_line.as_deref(), Some("DTSTART;TZID=Europe/Rome:20261105T090000"));
        assert_eq!(parsed.end_line.as_deref(), Some("DTEND;TZID=Europe/Rome:20261105T100000"));
        assert_eq!(parsed.sequence, 4);
        assert_eq!(parsed.summary_full, "Pianificazione trimestrale con il team allargato e i fornitori esterni");
    }

    let all_day = call(&mut engine, "message_invite", json!({ "id": "msg_allday" }))["invite"].clone();
    assert_eq!(all_day["start"], json!("2026-11-05"));
    let draft = call(&mut engine, "invite_reply_draft", json!({ "message": "msg_allday", "response": "accept" }));
    let attachment = draft["draft"]["attachments"][0]["id"].as_str().expect("attachment").to_owned();
    let bytes = engine.attachment_bytes(&attachment).expect("all-day calendar bytes");
    let parsed = postino_sync::parse_invite(&bytes).expect("the all-day reply parses back");
    assert_eq!(parsed.start_line.as_deref(), Some("DTSTART;VALUE=DATE:20261105"));
    assert_eq!(parsed.end_line.as_deref(), Some("DTEND;VALUE=DATE:20261106"));
}

#[test]
fn utf8_and_escaped_fields_survive_the_parse_and_the_reply() {
    let fixture = Fixture::new();
    fixture.seed();
    let mut engine = Engine::open(fixture.open()).expect("engine");
    let invite = call(&mut engine, "message_invite", json!({ "id": "msg_utf8" }))["invite"].clone();
    assert_eq!(invite["summary"], json!("Cena, tè; caffè — ünïcode"));
    assert_eq!(invite["location"], json!("Sala, piano 1; online"));
    let draft = call(&mut engine, "invite_reply_draft", json!({ "message": "msg_utf8", "response": "decline" }));
    let attachment = draft["draft"]["attachments"][0]["id"].as_str().expect("attachment").to_owned();
    let bytes = engine.attachment_bytes(&attachment).expect("calendar bytes");
    let parsed = postino_sync::parse_invite(&bytes).expect("reply parses back");
    assert_eq!(parsed.summary_full, "Cena, tè; caffè — ünïcode");
}

#[test]
fn the_calendar_part_is_found_by_name_without_loading_other_attachments() {
    let fixture = Fixture::new();
    fixture.seed();
    let mut engine = Engine::open(fixture.open()).expect("engine");
    let fallback = call(&mut engine, "message_invite", json!({ "id": "msg_fallback" }))["invite"].clone();
    assert_eq!(fallback["summary"], json!("Revisione preventivo"));
    let unrelated = call(&mut engine, "message_invite", json!({ "id": "msg_unrelated" }))["invite"].clone();
    assert_eq!(unrelated["can_reply"], json!(true), "a 10 MiB PDF ahead of the calendar is skipped, not read");
    for id in ["msg_missing", "msg_corrupt"] {
        assert_eq!(call(&mut engine, "message_invite", json!({ "id": id }))["invite"], Value::Null, "{id}");
        let error = ops::dispatch(&mut engine, "invite_reply_draft", json!({ "message": id, "response": "accept" })).expect_err(id);
        assert!(error.to_string().contains("no readable text/calendar invite"), "{id}: {error}");
    }
}

#[test]
fn only_received_mail_offers_a_live_reply() {
    let fixture = Fixture::new();
    fixture.seed();
    let mut engine = Engine::open(fixture.open()).expect("engine");
    let sent = call(&mut engine, "message_invite", json!({ "id": "msg_sent" }))["invite"].clone();
    assert_eq!(sent["valid"], json!(true));
    assert_eq!(sent["method"], json!("request"));
    assert_eq!(sent["can_reply"], json!(false), "a Sent copy is an outgoing record");
    let error = ops::dispatch(&mut engine, "invite_reply_draft", json!({ "message": "msg_sent", "response": "accept" })).expect_err("sent");
    assert!(error.to_string().contains("only a received invitation can be answered"), "{error}");
}

#[test]
fn cancel_reply_missing_organizer_and_malformed_parts_are_not_answered() {
    let fixture = Fixture::new();
    fixture.seed();
    let mut engine = Engine::open(fixture.open()).expect("engine");

    let cancelled = call(&mut engine, "message_invite", json!({ "id": "msg_cancel" }))["invite"].clone();
    assert_eq!(cancelled["method"], json!("cancel"));
    assert_eq!(cancelled["cancelled"], json!(true));
    assert_eq!(cancelled["can_reply"], json!(false));

    let answered = call(&mut engine, "message_invite", json!({ "id": "msg_reply" }))["invite"].clone();
    assert_eq!(answered["method"], json!("reply"));
    assert_eq!(answered["can_reply"], json!(false));

    let missing = call(&mut engine, "message_invite", json!({ "id": "msg_noorganizer" }))["invite"].clone();
    assert_eq!(missing["valid"], json!(true));
    assert_eq!(missing["organizer"], Value::Null);
    assert_eq!(missing["can_reply"], json!(false));

    for id in ["msg_broken", "msg_big"] {
        let unavailable = call(&mut engine, "message_invite", json!({ "id": id }));
        assert_eq!(unavailable["invite"], Value::Null, "{id} must fall back to the attachment");
    }
    for (id, reason) in [
        ("msg_broken", "no readable text/calendar invite"),
        ("msg_big", "no readable text/calendar invite"),
        ("msg_missing", "no readable text/calendar invite"),
        ("msg_corrupt", "no readable text/calendar invite"),
        ("msg_cancel", "only a valid METHOD:REQUEST with an organizer can be answered"),
        ("msg_reply", "only a valid METHOD:REQUEST with an organizer can be answered"),
        ("msg_noorganizer", "the invite has no organizer"),
        ("msg_sent", "only a received invitation can be answered"),
    ] {
        let error = ops::dispatch(&mut engine, "invite_reply_draft", json!({ "message": id, "response": "tentative" })).expect_err(reason);
        assert!(error.to_string().contains(reason), "{id}: {error}");
    }

    let declined = call(&mut engine, "invite_reply_draft", json!({ "message": "msg_request", "response": "decline" }));
    assert_eq!(declined["draft"]["summary"]["subject"], json!("Rifiutato: Revisione preventivo"));
    let attachment = declined["draft"]["attachments"][0]["id"].as_str().expect("attachment").to_owned();
    let bytes = engine.attachment_bytes(&attachment).expect("calendar bytes");
    assert!(String::from_utf8(bytes).expect("utf-8").contains("PARTSTAT=DECLINED"));
}

#[test]
fn both_operations_are_in_the_registry_with_honest_annotations() {
    let defs = ops::op_defs();
    let read = defs.iter().find(|def| def.name == "message_invite").expect("message_invite");
    assert!(read.read_only);
    assert!(!read.destructive && !read.sensitive);
    assert_eq!(read.input_schema["required"], json!(["id"]));
    assert_eq!(read.input_schema["properties"]["id"]["type"], json!("string"));

    let write = defs.iter().find(|def| def.name == "invite_reply_draft").expect("invite_reply_draft");
    assert!(!write.read_only);
    assert!(!write.destructive && !write.sensitive);
    assert_eq!(write.input_schema["required"], json!(["message", "response"]));
    let response = &write.input_schema["properties"]["response"];
    let variants = match response["enum"].as_array() {
        Some(variants) => variants.clone(),
        None => {
            let name = response["$ref"].as_str().expect("response schema").rsplit('/').next().expect("definition name");
            write.input_schema["$defs"][name]["enum"].as_array().expect("response enum").clone()
        }
    };
    let variants = variants.iter().filter_map(Value::as_str).collect::<Vec<_>>();
    assert_eq!(variants, ["accept", "tentative", "decline"]);
    assert_eq!(InviteResponse::Accept.partstat(), "ACCEPTED");
}
