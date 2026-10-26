//! FTS5 search: index lifecycle, filters, ranking, bounds and the encrypted-file invariant.

use std::path::Path;

use postino_store::{
    Account, Address, Attachment, Folder, FolderKind, Message, MessageSummary, SearchExpr, SearchFilter, SearchSort, Store, StoreKey,
};
use rusqlite::Connection;

fn random_key() -> StoreKey {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).expect("OS random source");
    StoreKey::from_bytes(bytes)
}

fn raw_open(path: &Path, key: &StoreKey) -> Connection {
    let connection = Connection::open(path).expect("raw open");
    let hex = key.as_bytes().iter().map(|byte| format!("{byte:02x}")).collect::<String>();
    connection.execute_batch(&format!("PRAGMA key = \"x'{hex}'\";")).expect("apply the key");
    connection
}

struct Fixture {
    dir: tempfile::TempDir,
    key: StoreKey,
}

impl Fixture {
    fn new() -> Self {
        Self { dir: tempfile::tempdir().expect("temp dir"), key: random_key() }
    }

    fn path(&self) -> std::path::PathBuf {
        self.dir.path().join("mail.db")
    }

    fn open(&self) -> Store {
        Store::open(&self.path(), &self.key).expect("store opens")
    }

    fn seeded(&self) -> Store {
        let store = self.open();
        store
            .upsert_account(&Account {
                id: "acc_1".to_owned(),
                name: "Studio".to_owned(),
                email: "studio@example.com".to_owned(),
                provider: "demo".to_owned(),
                signature: String::new(),
                created_at: 1_700_000_000_000,
            })
            .expect("account");
        store
            .upsert_folder(&Folder {
                id: "inbox".to_owned(),
                account_id: "acc_1".to_owned(),
                name: "Posta in arrivo".to_owned(),
                kind: FolderKind::Inbox,
                unread: 0,
                total: 0,
                uid_validity: Some(1),
                uid_next: Some(2),
                last_sync_at: None,
            })
            .expect("folder");
        store
            .upsert_folder(&Folder {
                id: "archive".to_owned(),
                account_id: "acc_1".to_owned(),
                name: "Archivio".to_owned(),
                kind: FolderKind::Archive,
                unread: 0,
                total: 0,
                uid_validity: Some(1),
                uid_next: Some(2),
                last_sync_at: None,
            })
            .expect("folder");
        store
    }
}

fn message(id: &str, subject: &str, body: &str, date: i64) -> Message {
    Message {
        summary: MessageSummary {
            id: id.to_owned(),
            account_id: "acc_1".to_owned(),
            folder_id: "inbox".to_owned(),
            thread_id: format!("thr_{id}"),
            from: Address::new("Giulia Rossi", "giulia@example.com"),
            to: vec![Address::new("Studio", "studio@example.com")],
            subject: subject.to_owned(),
            snippet: body.chars().take(80).collect(),
            date,
            unread: true,
            starred: false,
            has_attachments: false,
            labels: Vec::new(),
        },
        uid: Some(1),
        message_id: Some(format!("<{id}@example.com>")),
        cc: Vec::new(),
        bcc: Vec::new(),
        body_text: body.to_owned(),
        body_html: Some("<p>html must never reach a hit</p>".to_owned()),
        remote_images: 0,
        size: Some(body.len() as u64),
    }
}

fn insert(store: &Store, row: &Message) {
    store.upsert_synced_message(row, &[]).expect("insert message");
}

fn search(store: &Store, expr: SearchExpr) -> postino_store::SearchPage {
    store.search(&SearchFilter { expr: Some(expr), ..SearchFilter::default() }).expect("search")
}

#[test]
fn the_index_tracks_insert_update_and_delete_but_not_flags() {
    let fixture = Fixture::new();
    let store = fixture.seeded();
    insert(&store, &message("msg_1", "Verbale riunione", "Il verbale della riunione", 1_000));
    assert_eq!(search(&store, SearchExpr::Match("\"verbale\"".to_owned())).total, 1);
    assert_eq!(store.search_status().expect("status").indexed, 1);

    store.set_flags(&["msg_1".to_owned()], postino_store::FlagUpdate { unread: Some(false), starred: Some(true) }).expect("flags");
    assert_eq!(search(&store, SearchExpr::Match("\"verbale\"".to_owned())).total, 1);

    // A body update deletes and reinserts the FTS row: the old term disappears, the new one lands.
    let mut updated = message("msg_1", "Bilancio", "Il bilancio annuale", 1_000);
    updated.summary.unread = false;
    updated.summary.starred = true;
    insert(&store, &updated);
    assert_eq!(search(&store, SearchExpr::Match("\"verbale\"".to_owned())).total, 0);
    assert_eq!(search(&store, SearchExpr::Match("\"bilancio\"".to_owned())).total, 1);

    store.delete_message("msg_1").expect("delete");
    assert_eq!(search(&store, SearchExpr::Match("\"bilancio\"".to_owned())).total, 0);
    assert_eq!(store.search_status().expect("status").indexed, 0, "the delete trigger left an FTS document");
}

#[test]
fn people_subject_prefix_attachment_names_and_unicode_are_searchable() {
    let fixture = Fixture::new();
    let store = fixture.seeded();
    let mut row = message("msg_1", "Fattura Città", "Importo dovuto", 1_000);
    row.cc = vec![Address::new("Marco Bianchi", "marco@example.it")];
    insert(&store, &row);
    store
        .replace_attachments(
            "msg_1",
            &[Attachment {
                id: "att_1".to_owned(),
                filename: "contratto.pdf".to_owned(),
                mime: "application/pdf".to_owned(),
                size: 10,
                blob_hash: None,
            }],
        )
        .expect("attachment");
    // The attachment-name update reindexes the row through the trigger.
    assert_eq!(search(&store, SearchExpr::Match("\"contratto\"".to_owned())).total, 1);
    assert_eq!(search(&store, SearchExpr::Match("search_sender : \"giulia\"".to_owned())).total, 1);
    assert_eq!(search(&store, SearchExpr::Match("search_cc : \"marco\"".to_owned())).total, 1);
    assert_eq!(search(&store, SearchExpr::Match("\"citta\"".to_owned())).total, 1, "remove_diacritics 2");
    assert_eq!(search(&store, SearchExpr::Match("\"fatt\"*".to_owned())).total, 1, "prefix index");
    assert_eq!(search(&store, SearchExpr::Match("subject : \"fattura\"".to_owned())).total, 1);
}

#[test]
fn filters_ranking_and_date_order_are_the_documented_ones() {
    let fixture = Fixture::new();
    let store = fixture.seeded();
    let mut subject_hit = message("msg_subject", "Fattura di marzo", "corpo senza termine", 1_000);
    subject_hit.summary.starred = true;
    subject_hit.size = Some(10);
    insert(&store, &subject_hit);
    let mut body_hit = message("msg_body", "Nota", "la fattura allegata", 2_000);
    body_hit.summary.unread = false;
    body_hit.size = Some(200_000);
    insert(&store, &body_hit);
    let mut old_hit = message("msg_old", "Fattura vecchia", "corpo", 500);
    old_hit.size = Some(100);
    insert(&store, &old_hit);

    let page = search(&store, SearchExpr::Match("\"fattura\"".to_owned()));
    assert_eq!(page.total, 3);
    let ids = page.hits.iter().map(|hit| hit.message.id.as_str()).collect::<Vec<_>>();
    assert!(
        ids.iter().position(|id| *id == "msg_subject") < ids.iter().position(|id| *id == "msg_body"),
        "a subject weight must outrank a body-only hit: {ids:?}"
    );

    let date_page = store
        .search(&SearchFilter { expr: Some(SearchExpr::Match("\"fattura\"".to_owned())), sort: SearchSort::Date, ..SearchFilter::default() })
        .expect("date search");
    assert_eq!(date_page.hits.iter().map(|hit| hit.message.id.as_str()).collect::<Vec<_>>(), vec!["msg_body", "msg_subject", "msg_old"]);

    let unread = search(&store, SearchExpr::And(vec![SearchExpr::Match("\"fattura\"".to_owned()), SearchExpr::Unread(true)]));
    assert_eq!(unread.total, 2);
    let starred = search(&store, SearchExpr::And(vec![SearchExpr::Match("\"fattura\"".to_owned()), SearchExpr::Starred(true)]));
    assert_eq!(starred.total, 1);
    let or_filter = store
        .search(&SearchFilter { expr: Some(SearchExpr::Or(vec![SearchExpr::Unread(true), SearchExpr::Starred(true)])), ..SearchFilter::default() })
        .expect("filter-only OR");
    assert_eq!(or_filter.total, 2, "unread or starred: the subject hit is both, so the union is two rows");
    let large = search(&store, SearchExpr::And(vec![SearchExpr::Match("\"fattura\"".to_owned()), SearchExpr::Larger(1_000)]));
    assert_eq!(large.total, 1);
    assert_eq!(large.hits[0].message.id, "msg_body");
    let between =
        search(&store, SearchExpr::And(vec![SearchExpr::Match("\"fattura\"".to_owned()), SearchExpr::After(900), SearchExpr::Before(1_500)]));
    assert_eq!(between.total, 1);
    assert_eq!(between.hits[0].message.id, "msg_subject");
    let folder = search(&store, SearchExpr::And(vec![SearchExpr::Match("\"fattura\"".to_owned()), SearchExpr::Folder("archive".to_owned())]));
    assert_eq!(folder.total, 0);
    let not_body = search(
        &store,
        SearchExpr::And(vec![SearchExpr::Match("\"fattura\"".to_owned()), SearchExpr::Not(Box::new(SearchExpr::Match("\"allegata\"".to_owned())))]),
    );
    assert_eq!(not_body.total, 2, "NOT text is a filter, not a ranking term");

    // Equal rank falls back to date DESC, id ASC: insert two otherwise identical rows.
    insert(&store, &message("msg_tie_a", "Fattura", "corpo", 10));
    insert(&store, &message("msg_tie_b", "Fattura", "corpo", 20));
    let tied = search(&store, SearchExpr::Match("\"fattura\"".to_owned()));
    let ids = tied.hits.iter().map(|hit| hit.message.id.as_str()).collect::<Vec<_>>();
    let a = ids.iter().position(|id| *id == "msg_tie_a");
    let b = ids.iter().position(|id| *id == "msg_tie_b");
    assert!(b < a, "the newer identical-rank message must come first: {ids:?}");

    store.apply_label("lbl_1", &["msg_subject".to_owned()]).expect_err("unknown label");
}

#[test]
fn label_filter_and_pagination_do_not_duplicate_a_row() {
    let fixture = Fixture::new();
    let store = fixture.seeded();
    store
        .create_label(&postino_store::Label { id: "lbl_1".to_owned(), account_id: None, name: "Lavoro".to_owned(), color: "forest".to_owned() })
        .expect("label");
    for index in 0..5 {
        let mut row = message(&format!("msg_{index}"), "Rapporto", "rapporto mensile", i64::from(index));
        if index == 2 {
            row.summary.labels = vec!["lbl_1".to_owned()];
            row.summary.folder_id = "archive".to_owned();
            store.upsert_synced_message(&row, &[]).expect("message");
            store.apply_label("lbl_1", &["msg_2".to_owned()]).expect("apply");
        } else {
            insert(&store, &row);
        }
    }
    let labelled = store
        .search(&SearchFilter {
            expr: Some(SearchExpr::And(vec![SearchExpr::Match("\"rapporto\"".to_owned()), SearchExpr::Label("lbl_1".to_owned())])),
            ..SearchFilter::default()
        })
        .expect("search");
    assert_eq!(labelled.total, 1);
    assert_eq!(labelled.hits[0].message.id, "msg_2");

    let first = store
        .search(&SearchFilter { expr: Some(SearchExpr::Match("\"rapporto\"".to_owned())), limit: 2, ..SearchFilter::default() })
        .expect("page 1");
    let second = store
        .search(&SearchFilter { expr: Some(SearchExpr::Match("\"rapporto\"".to_owned())), limit: 2, offset: 2, ..SearchFilter::default() })
        .expect("page 2");
    assert_eq!(first.total, 5);
    assert_eq!(first.hits.len(), 2);
    assert_eq!(second.hits.len(), 2);
    assert!(first.hits.iter().all(|hit| second.hits.iter().all(|other| other.message.id != hit.message.id)));
}

#[test]
fn the_total_is_exact_to_the_cap_and_then_flagged() {
    let fixture = Fixture::new();
    let store = fixture.seeded();
    drop(store);
    let mut connection = raw_open(&fixture.path(), &fixture.key);
    let tx = connection.transaction().expect("raw transaction");
    {
        let mut insert = tx
            .prepare(
                "INSERT INTO messages(id, account_id, folder_id, thread_id, from_name, from_email, to_json, cc_json, bcc_json,
                     subject, snippet, date, unread, starred, has_attachments, body_text)
                 VALUES(?1, 'acc_1', 'inbox', ?1, 'a', 'a@example.com', '[]', '[]', '[]', 'x', '', 1, 0, 0, 0, 'x')",
            )
            .expect("prepare");
        for index in 0..=postino_store::SEARCH_TOTAL_CAP {
            insert.execute([format!("cap_{index}")]).expect("insert");
        }
    }
    tx.commit().expect("commit");
    drop(connection);
    let store = fixture.open();
    let page = store.search(&SearchFilter { expr: Some(SearchExpr::Match("\"x\"".to_owned())), ..SearchFilter::default() }).expect("search");
    assert_eq!(page.total, postino_store::SEARCH_TOTAL_CAP);
    assert!(page.total_capped, "10001 matches must report a capped total");
    assert_eq!(page.hits.len(), 50);
}

#[test]
fn threading_headers_are_stored_with_the_message() {
    let fixture = Fixture::new();
    let store = fixture.seeded();
    let row = message("msg_1", "Re: Verbale", "corpo", 1_000);
    store
        .upsert_synced_message_with_refs(&row, &[], Some("parent@example.com"), &["root@example.com".to_owned(), "parent@example.com".to_owned()])
        .expect("message with refs");
    drop(store);
    let connection = raw_open(&fixture.path(), &fixture.key);
    let (parent, references, ingested): (Option<String>, Option<String>, i64) = connection
        .query_row("SELECT in_reply_to, references_json, ingested_at FROM messages WHERE id = 'msg_1'", [], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })
        .expect("headers");
    assert_eq!(parent.as_deref(), Some("parent@example.com"));
    assert_eq!(references.as_deref(), Some("[\"root@example.com\",\"parent@example.com\"]"));
    assert!(ingested > 0, "ingested_at must be stamped at insert");
}

#[test]
fn deleting_an_account_removes_its_search_documents() {
    let fixture = Fixture::new();
    let store = fixture.seeded();
    insert(&store, &message("msg_1", "Verbale", "corpo", 1_000));
    assert_eq!(store.search_status().expect("status").indexed, 1);
    store.delete_account("acc_1").expect("delete account");
    let status = store.search_status().expect("status");
    assert_eq!((status.indexed, status.total), (0, 0), "the cascade left search documents or message rows behind");
    assert_eq!(search(&store, SearchExpr::Match("\"verbale\"".to_owned())).total, 0);
}

#[test]
fn a_hostile_match_expression_is_bound_and_cannot_change_the_schema() {
    let fixture = Fixture::new();
    let store = fixture.seeded();
    insert(&store, &message("msg_1", "Nota", "corpo", 1_000));
    let page = search(&store, SearchExpr::Match("\"x\"\" ); DROP TABLE messages; --\"".to_owned()));
    assert_eq!(page.total, 0);
    assert_eq!(store.list_all_summaries().expect("messages still exist").len(), 1);
}

#[test]
fn the_indexed_body_is_bounded_and_no_html_is_returned() {
    let fixture = Fixture::new();
    let store = fixture.seeded();
    let mut row = message("msg_1", "Grande", &format!("{}{}", "a".repeat(2 * 1024 * 1024), " termine-finale"), 1_000);
    row.body_html = Some("<p>segreto html</p>".to_owned());
    insert(&store, &row);
    assert_eq!(search(&store, SearchExpr::Match("\"termine-finale\"".to_owned())).total, 0, "the body index must be bounded to 1 MiB");
    let hit = search(&store, SearchExpr::Match("\"aaaa\"*".to_owned()));
    assert_eq!(hit.total, 1);
    assert!(!hit.hits[0].body_text.contains("segreto"), "search must read body_text, never body_html");
    assert!(hit.hits[0].body_text.chars().count() <= 16_384);
}

#[test]
fn reindex_rebuilds_an_emptied_index_and_status_reports_drift() {
    let fixture = Fixture::new();
    let store = fixture.seeded();
    insert(&store, &message("msg_1", "Verbale", "corpo", 1_000));
    let status = store.search_status().expect("status");
    assert!(status.ready);
    assert_eq!(status.indexed, 1);
    assert!(status.size_bytes > 0);
    assert!(status.updated_at > 0);
    {
        let connection = raw_open(&fixture.path(), &fixture.key);
        connection.execute("DELETE FROM message_fts", []).expect("empty the index");
    }
    let drift = store.search_status().expect("drift status");
    assert!(!drift.ready, "an emptied index must be reported as drift: {drift:?}");
    assert_eq!(drift.indexed, 0);
    let outcome = store.search_reindex().expect("reindex");
    assert_eq!(outcome.documents, 1);
    assert_eq!(search(&store, SearchExpr::Match("\"verbale\"".to_owned())).total, 1);
    drop(store);
    let reopened = fixture.open();
    assert!(reopened.search_status().expect("reopen status").ready, "a reopen repairs a ready count mismatch");
}

#[test]
fn the_shadow_tables_are_inside_the_encrypted_database_and_there_is_no_sidecar() {
    let fixture = Fixture::new();
    let store = fixture.seeded();
    insert(&store, &message("msg_1", "Riservato", "parola-segreta", 1_000));
    drop(store);
    let connection = raw_open(&fixture.path(), &fixture.key);
    let shadow: Vec<String> = connection
        .prepare("SELECT name FROM sqlite_master WHERE type = 'table' AND name LIKE 'message_fts_%' ORDER BY name")
        .expect("prepare")
        .query_map([], |row| row.get(0))
        .expect("query")
        .collect::<rusqlite::Result<_>>()
        .expect("rows");
    assert_eq!(shadow, vec!["message_fts_config", "message_fts_data", "message_fts_docsize", "message_fts_idx"]);
    drop(connection);
    let entries = std::fs::read_dir(fixture.dir.path())
        .expect("read dir")
        .map(|entry| entry.expect("entry").file_name().to_string_lossy().to_string())
        .collect::<Vec<_>>();
    assert!(entries.iter().all(|name| !name.contains("fts")), "FTS must have no sidecar file: {entries:?}");
    let bytes = std::fs::read(fixture.path()).expect("read database");
    let needle = b"parola-segreta";
    assert!(!bytes.windows(needle.len()).any(|window| window == needle), "the index leaked plaintext");
}
