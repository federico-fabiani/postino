//! Repository round trips, counts, batch atomicity, reopen persistence and the no-plaintext proof.

use std::path::{Path, PathBuf};

use postino_store::{
    Account, Address, Attachment, DraftUpdate, FlagUpdate, Folder, FolderKind, Label, Message, MessageFilter, MessageSummary, Store, StoreError,
    StoreKey,
};

const ACCOUNT: &str = "acc_1";
const INBOX: &str = "inbox";
const ARCHIVE: &str = "archive";

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

    fn path(&self) -> PathBuf {
        self.dir.path().join("store.db")
    }

    fn open(&self) -> Store {
        Store::open(&self.path(), &self.key).expect("open the store")
    }
}

fn address(name: &str, email: &str) -> Address {
    Address::new(name, email)
}

fn sample_account() -> Account {
    Account {
        id: ACCOUNT.to_owned(),
        name: "Studio Verdi".to_owned(),
        email: "studio@example.com".to_owned(),
        provider: "demo".to_owned(),
        signature: "Cordiali saluti".to_owned(),
        created_at: 1_700_000_000_000,
    }
}

fn folder(id: &str, name: &str, kind: FolderKind) -> Folder {
    Folder {
        id: id.to_owned(),
        account_id: ACCOUNT.to_owned(),
        name: name.to_owned(),
        kind,
        unread: 0,
        total: 0,
        uid_validity: Some(1),
        uid_next: Some(10),
        last_sync_at: Some(1_700_000_000_000),
    }
}

fn sample_message(id: &str, folder_id: &str, subject: &str, body: &str, date: i64) -> Message {
    Message {
        summary: MessageSummary {
            id: id.to_owned(),
            account_id: ACCOUNT.to_owned(),
            folder_id: folder_id.to_owned(),
            thread_id: format!("thread_{id}"),
            from: address("Federica Bianchi", "federica@example.com"),
            to: vec![address("Giulia", "giulia@example.com"), address("Marco", "marco@example.com")],
            subject: subject.to_owned(),
            snippet: body.chars().take(40).collect(),
            date,
            unread: true,
            starred: false,
            has_attachments: false,
            labels: Vec::new(),
        },
        uid: Some(1),
        message_id: Some(format!("<{id}@example.com>")),
        cc: vec![address("Studio Verdi", "studio@example.it")],
        bcc: Vec::new(),
        body_text: body.to_owned(),
        body_html: None,
        remote_images: 0,
        size: Some(4096),
    }
}

fn seed(store: &Store) {
    store.upsert_account(&sample_account()).expect("account");
    store.upsert_folder(&folder(INBOX, "Posta in arrivo", FolderKind::Inbox)).expect("inbox");
    store.upsert_folder(&folder(ARCHIVE, "Archivio", FolderKind::Archive)).expect("archive");
    store
        .upsert_message(&sample_message("msg_1", INBOX, "Rapporto mensile", "Buongiorno, ecco il rapporto di ottobre.", 1_700_000_100_000))
        .expect("message");
}

fn ids(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_owned()).collect()
}

#[test]
fn draft_ids_are_allocated_by_the_store_and_never_collide() {
    let fixture = Fixture::new();
    let first_connection = fixture.open();
    seed(&first_connection);
    assert!(first_connection.list_drafts(None).expect("drafts").is_empty());

    // Two connections, interleaved: the allocator is a store transaction, so no id is handed out twice.
    let second_connection = fixture.open();
    let a = first_connection.allocate_draft_id().expect("id a");
    let b = second_connection.allocate_draft_id().expect("id b");
    let c = first_connection.allocate_draft_id().expect("id c");
    assert_eq!(a, "draft_1");
    assert_eq!(b, "draft_2");
    assert_eq!(c, "draft_3");
    assert_ne!(a, b);
    assert_ne!(b, c);

    // An existing draft is kept and the sequence continues above it after a reopen.
    let mut message = sample_message(&c, INBOX, "Bozza", "corpo", 1_700_000_200_000);
    message.summary.unread = false;
    first_connection.upsert_folder(&folder("drafts", "Bozze", FolderKind::Drafts)).expect("drafts folder");
    message.summary.folder_id = "drafts".to_owned();
    first_connection.save_draft_message(&message, &[]).expect("save");
    drop(first_connection);
    drop(second_connection);
    let reopened = fixture.open();
    assert_eq!(reopened.list_drafts(None).expect("drafts").len(), 1);
    assert_eq!(reopened.allocate_draft_id().expect("after the reopen"), "draft_4");
}

#[test]
fn round_trip_account_folder_message_attachment_and_labels() {
    let fixture = Fixture::new();
    let store = fixture.open();
    seed(&store);

    let mut message = sample_message("msg_1", INBOX, "Preventivo per il sito", "Ciao, ti mando il preventivo.", 1_700_000_100_000);
    message.summary.starred = true;
    message.size = Some(8192);
    message.remote_images = 3;
    store.upsert_message(&message).expect("message");

    store
        .create_label(&Label { id: "lbl_1".to_owned(), account_id: Some(ACCOUNT.to_owned()), name: "Lavoro".to_owned(), color: "forest".to_owned() })
        .expect("account label");
    store
        .create_label(&Label { id: "lbl_2".to_owned(), account_id: None, name: "Importante".to_owned(), color: "gold".to_owned() })
        .expect("global label");
    store
        .replace_attachments(
            "msg_1",
            &[Attachment {
                id: "att_1".to_owned(),
                filename: "preventivo.pdf".to_owned(),
                mime: "application/pdf".to_owned(),
                size: 2048,
                blob_hash: Some("blake3:0123456789abcdef".to_owned()),
            }],
        )
        .expect("attachment");
    assert_eq!(store.apply_label("lbl_1", &ids(&["msg_1"])).expect("apply"), 1);
    assert_eq!(store.apply_label("lbl_2", &ids(&["msg_1"])).expect("apply"), 1);

    let loaded = store.get_message("msg_1").expect("get").expect("message exists");
    assert_eq!(loaded.summary.subject, "Preventivo per il sito");
    assert_eq!(loaded.summary.to.len(), 2);
    assert_eq!(loaded.summary.to[1].name, "Marco");
    assert_eq!(loaded.summary.to[1].email, "marco@example.com");
    assert_eq!(loaded.cc.len(), 1);
    assert_eq!(loaded.bcc.len(), 0);
    assert_eq!(loaded.body_text, "Ciao, ti mando il preventivo.");
    assert_eq!(loaded.summary.labels, vec!["lbl_1", "lbl_2"]);
    assert!(loaded.summary.has_attachments, "replace_attachments did not set the flag");
    assert_eq!(loaded.size, Some(8192));
    assert_eq!(loaded.remote_images, 3, "the stored remote-image count did not round-trip");
    assert_eq!(loaded.uid, Some(1));
    assert_eq!(loaded.message_id.as_deref(), Some("<msg_1@example.com>"));

    let page = store.list_messages(&MessageFilter { folder_id: Some(INBOX.to_owned()), ..Default::default() }).expect("list");
    assert_eq!((page.total, page.unread), (1, 1));
    assert_eq!(page.messages.len(), 1);
    assert_eq!(page.messages[0].labels, vec!["lbl_1", "lbl_2"]);

    let by_label = store.messages_by_label("lbl_1").expect("label view");
    assert_eq!(by_label.len(), 1);
    assert_eq!(by_label[0].id, "msg_1");

    let labels = store.list_labels(Some(ACCOUNT)).expect("labels");
    assert_eq!(labels.len(), 2);
    assert!(labels.iter().all(|label| label.message_count == 1 && label.unread_count == 1), "{labels:?}");

    let attachments = store.attachments("msg_1").expect("attachments");
    assert_eq!(attachments.len(), 1);
    assert_eq!(attachments[0].filename, "preventivo.pdf");
    assert_eq!(attachments[0].size, 2048);

    // Upserting the message again preserves label assignments and attachments.
    message.summary.subject = "Preventivo per il sito (rev 2)".to_owned();
    message.summary.labels.clear();
    message.remote_images = 1;
    store.upsert_message(&message).expect("re-upsert");
    let refreshed = store.get_message("msg_1").expect("get").expect("message exists");
    assert_eq!(refreshed.summary.subject, "Preventivo per il sito (rev 2)");
    assert_eq!(refreshed.remote_images, 1, "a body refresh did not replace the remote-image count");
    assert_eq!(refreshed.summary.labels, vec!["lbl_1", "lbl_2"], "the upsert dropped label assignments");
    assert_eq!(store.attachments("msg_1").expect("attachments").len(), 1, "the upsert dropped attachments");

    // Replacement is explicit and keeps `has_attachments` in step.
    store
        .replace_attachments(
            "msg_1",
            &[
                Attachment {
                    id: "att_2".to_owned(),
                    filename: "contratto.pdf".to_owned(),
                    mime: "application/pdf".to_owned(),
                    size: 10,
                    blob_hash: None,
                },
                Attachment { id: "att_3".to_owned(), filename: "foto.png".to_owned(), mime: "image/png".to_owned(), size: 20, blob_hash: None },
            ],
        )
        .expect("replace");
    assert_eq!(store.attachments("msg_1").expect("attachments").len(), 2);
    store.replace_attachments("msg_1", &[]).expect("clear");
    assert!(store.attachments("msg_1").expect("attachments").is_empty());
    assert!(!store.get_message("msg_1").expect("get").expect("message").summary.has_attachments);

    println!(
        "repository round-trip: accounts={} folders={} message={} recipients={} labels={} attachments_after_clear={} page_total={}",
        store.list_accounts().expect("accounts").len(),
        store.list_folders(None).expect("folders").len(),
        refreshed.summary.id,
        refreshed.summary.to.len(),
        refreshed.summary.labels.len(),
        store.attachments("msg_1").expect("attachments").len(),
        page.total
    );
}

#[test]
fn counts_follow_read_move_and_delete() {
    let fixture = Fixture::new();
    let store = fixture.open();
    seed(&store);
    store
        .create_label(&Label { id: "lbl_1".to_owned(), account_id: Some(ACCOUNT.to_owned()), name: "Lavoro".to_owned(), color: "forest".to_owned() })
        .expect("label");
    store.apply_label("lbl_1", &ids(&["msg_1"])).expect("apply");
    store
        .replace_attachments(
            "msg_1",
            &[Attachment {
                id: "att_1".to_owned(),
                filename: "rapporto.pdf".to_owned(),
                mime: "application/pdf".to_owned(),
                size: 5,
                blob_hash: None,
            }],
        )
        .expect("attachment");

    assert_eq!(store.counts(Some(INBOX)).expect("counts"), postino_store::MessageCounts { total: 1, unread: 1 });
    assert_eq!(store.get_folder(INBOX).expect("folder").expect("inbox").unread, 1);

    assert_eq!(store.set_flags(&ids(&["msg_1"]), FlagUpdate { unread: Some(false), starred: Some(true) }).expect("flag"), 1);
    assert_eq!(store.counts(Some(INBOX)).expect("counts"), postino_store::MessageCounts { total: 1, unread: 0 });
    let inbox = store.get_folder(INBOX).expect("folder").expect("inbox");
    assert_eq!((inbox.total, inbox.unread), (1, 0), "the folder count cache is stale");
    let flagged = store.get_message("msg_1").expect("get").expect("message");
    assert!(!flagged.summary.unread);
    assert!(flagged.summary.starred);

    assert_eq!(store.move_messages(&ids(&["msg_1"]), ARCHIVE).expect("move"), 1);
    assert_eq!(store.counts(Some(INBOX)).expect("counts"), postino_store::MessageCounts::default());
    assert_eq!(store.counts(Some(ARCHIVE)).expect("counts"), postino_store::MessageCounts { total: 1, unread: 0 });
    assert_eq!(store.get_folder(INBOX).expect("folder").expect("inbox").total, 0);
    assert_eq!(store.get_folder(ARCHIVE).expect("folder").expect("archive").total, 1);
    assert_eq!(store.get_message("msg_1").expect("get").expect("message").summary.folder_id, ARCHIVE);

    assert!(store.delete_message("msg_1").expect("delete"));
    assert!(!store.delete_message("msg_1").expect("delete again"), "delete is not idempotent");
    assert_eq!(store.counts(Some(ARCHIVE)).expect("counts"), postino_store::MessageCounts::default());
    assert_eq!(store.get_folder(ARCHIVE).expect("folder").expect("archive").unread, 0);
    assert!(store.get_message("msg_1").expect("get").is_none(), "the message survived deletion");

    // Re-creating the id must not inherit rows deleted by cascade.
    store.upsert_message(&sample_message("msg_1", INBOX, "Nuova", "Nuovo testo", 1_700_000_300_000)).expect("re-create");
    assert!(store.labels_for("msg_1").expect("labels").is_empty(), "label assignments survived the delete");
    assert!(store.attachments("msg_1").expect("attachments").is_empty(), "attachments survived the delete");

    // A message upsert that changes folder refreshes both folder caches.
    store.upsert_message(&sample_message("msg_1", ARCHIVE, "Spostata", "Testo", 1_700_000_400_000)).expect("upsert-move");
    assert_eq!(store.counts(Some(INBOX)).expect("counts"), postino_store::MessageCounts::default());
    assert_eq!(store.counts(Some(ARCHIVE)).expect("counts"), postino_store::MessageCounts { total: 1, unread: 1 });

    // A failed batch flag update must leave every row untouched.
    store.upsert_message(&sample_message("msg_2", INBOX, "Seconda", "Testo", 1_700_000_200_000)).expect("second message");
    let error = store.set_flags(&ids(&["msg_2", "missing"]), FlagUpdate { unread: Some(false), starred: None }).expect_err("a missing id must fail");
    assert!(matches!(error, StoreError::NotFound { entity: "message", .. }), "{error}");
    assert!(store.get_message("msg_2").expect("get").expect("message").summary.unread, "the batch was not rolled back");
}

#[test]
fn mailbox_hydration_reads_every_message_label_and_attachment() {
    let fixture = Fixture::new();
    let store = fixture.open();
    seed(&store);
    store.upsert_message(&sample_message("msg_2", INBOX, "Seconda", "Testo", 1_700_000_200_000)).expect("second message");
    let mut third = sample_message("msg_3", ARCHIVE, "Terza", "Altro testo", 1_700_000_300_000);
    third.body_html = Some("<p>Altro testo</p>".to_owned());
    third.summary.unread = false;
    third.summary.starred = true;
    store.upsert_message(&third).expect("third message");
    store
        .create_label(&Label { id: "lbl_1".to_owned(), account_id: Some(ACCOUNT.to_owned()), name: "Lavoro".to_owned(), color: "forest".to_owned() })
        .expect("label");
    store
        .create_label(&Label { id: "lbl_2".to_owned(), account_id: Some(ACCOUNT.to_owned()), name: "Casa".to_owned(), color: "gold".to_owned() })
        .expect("label");
    store.apply_label("lbl_2", &ids(&["msg_1"])).expect("apply lbl_2");
    store.apply_label("lbl_1", &ids(&["msg_1"])).expect("apply lbl_1");
    store
        .replace_attachments(
            "msg_1",
            &[
                Attachment { id: "att_1".to_owned(), filename: "primo.pdf".to_owned(), mime: "application/pdf".to_owned(), size: 7, blob_hash: None },
                Attachment { id: "att_2".to_owned(), filename: "secondo.png".to_owned(), mime: "image/png".to_owned(), size: 9, blob_hash: None },
            ],
        )
        .expect("attachments");

    let messages = store.list_all_messages().expect("hydrate messages");
    assert_eq!(messages.iter().map(|message| message.summary.id.as_str()).collect::<Vec<_>>(), vec!["msg_3", "msg_2", "msg_1"]);
    let first = messages.iter().find(|message| message.summary.id == "msg_1").expect("msg_1");
    assert_eq!(first.body_text, "Buongiorno, ecco il rapporto di ottobre.");
    assert_eq!(first.summary.labels, vec!["lbl_1", "lbl_2"], "labels arrive ordered by id");
    let archived = messages.iter().find(|message| message.summary.id == "msg_3").expect("msg_3");
    assert_eq!(archived.body_html.as_deref(), Some("<p>Altro testo</p>"));

    let attachments = store.list_all_attachments().expect("hydrate attachments");
    assert_eq!(attachments.len(), 1, "messages without attachments are absent");
    let first_attachments = attachments.get("msg_1").expect("msg_1 attachments");
    assert_eq!(first_attachments.iter().map(|attachment| attachment.filename.as_str()).collect::<Vec<_>>(), vec!["primo.pdf", "secondo.png"]);
    assert!(!attachments.contains_key("msg_2"));
}

#[test]
fn a_remote_image_count_is_bounded_on_write_and_on_read() {
    let fixture = Fixture::new();
    let store = fixture.open();
    seed(&store);

    let mut message = sample_message("msg_1", INBOX, "Notizie", "Corpo", 1_700_000_100_000);
    message.remote_images = 25_000;
    store.upsert_message(&message).expect("the write clamps instead of failing the CHECK");
    assert_eq!(store.get_message("msg_1").expect("get").expect("message").remote_images, 10_000);

    // A damaged row that bypassed the CHECK must not reach the reading pane unbounded either.
    drop(store);
    let connection = rusqlite::Connection::open(fixture.path()).expect("raw open");
    let hex = fixture.key.as_bytes().iter().map(|byte| format!("{byte:02x}")).collect::<String>();
    connection
        .execute_batch(&format!(
            "PRAGMA key = \"x'{hex}'\";
             PRAGMA ignore_check_constraints = ON;
             UPDATE messages SET remote_images = 20000 WHERE id = 'msg_1';"
        ))
        .expect("forge an out-of-range count");
    drop(connection);

    let reopened = fixture.open();
    assert_eq!(reopened.get_message("msg_1").expect("get").expect("message").remote_images, 10_000, "the read must clamp");
}

#[test]
fn recount_repairs_the_folder_cache() {
    let fixture = Fixture::new();
    let store = fixture.open();
    seed(&store);

    let mut inbox = store.get_folder(INBOX).expect("folder").expect("inbox");
    inbox.unread = 9;
    inbox.total = 42;
    store.upsert_folder(&inbox).expect("forge stale counts");
    assert_eq!(store.get_folder(INBOX).expect("folder").expect("inbox").total, 42);

    let counts = store.recount_folder(INBOX).expect("recount");
    assert_eq!(counts, postino_store::MessageCounts { total: 1, unread: 1 });
    let repaired = store.get_folder(INBOX).expect("folder").expect("inbox");
    assert_eq!((repaired.total, repaired.unread), (1, 1), "recount did not persist the cache");
    assert!(matches!(store.recount_folder("missing"), Err(StoreError::NotFound { entity: "folder", .. })));
}

#[test]
fn deleting_a_label_detaches_it_and_keeps_mail() {
    let fixture = Fixture::new();
    let store = fixture.open();
    seed(&store);
    store
        .create_label(&Label { id: "lbl_1".to_owned(), account_id: Some(ACCOUNT.to_owned()), name: "Lavoro".to_owned(), color: "forest".to_owned() })
        .expect("label");
    store
        .create_label(&Label { id: "lbl_2".to_owned(), account_id: Some(ACCOUNT.to_owned()), name: "Casa".to_owned(), color: "muted".to_owned() })
        .expect("label");
    store.apply_label("lbl_1", &ids(&["msg_1"])).expect("apply");
    store.apply_label("lbl_2", &ids(&["msg_1"])).expect("apply");

    let duplicate = store.update_label("lbl_2", Some("Lavoro"), None).expect_err("a duplicate rename must be refused");
    assert!(matches!(duplicate, StoreError::Duplicate { entity: "label", .. }), "{duplicate}");
    let renamed = store.update_label("lbl_2", Some("Casa e famiglia"), Some("gold")).expect("rename");
    assert_eq!(renamed.name, "Casa e famiglia");
    assert_eq!(renamed.color, "gold");
    assert_eq!(renamed.account_id.as_deref(), Some(ACCOUNT));

    assert!(store.delete_label("lbl_1").expect("delete label"));
    assert!(!store.delete_label("lbl_1").expect("delete again"));
    let message = store.get_message("msg_1").expect("get").expect("the mail is still there");
    assert_eq!(message.summary.labels, vec!["lbl_2"]);
    assert_eq!(store.messages_by_label("lbl_2").expect("label view").len(), 1);
}

#[test]
fn label_batches_are_idempotent_and_atomic() {
    let fixture = Fixture::new();
    let store = fixture.open();
    seed(&store);
    store.upsert_message(&sample_message("msg_2", INBOX, "Seconda", "Testo", 1_700_000_200_000)).expect("second message");
    store
        .create_label(&Label { id: "lbl_1".to_owned(), account_id: Some(ACCOUNT.to_owned()), name: "Lavoro".to_owned(), color: "forest".to_owned() })
        .expect("label");
    store
        .create_label(&Label { id: "lbl_2".to_owned(), account_id: Some(ACCOUNT.to_owned()), name: "Urgente".to_owned(), color: "danger".to_owned() })
        .expect("label");

    assert_eq!(store.apply_label("lbl_1", &ids(&["msg_1", "msg_2"])).expect("apply"), 2);
    assert_eq!(store.apply_label("lbl_1", &ids(&["msg_1", "msg_2"])).expect("apply again"), 0, "apply is not idempotent");
    assert_eq!(store.remove_label("lbl_1", &ids(&["msg_1"])).expect("remove"), 1);
    assert_eq!(store.remove_label("lbl_1", &ids(&["msg_1"])).expect("remove again"), 0);

    // One bad id rolls the whole batch back: msg_1 must not keep lbl_2.
    let error = store.apply_label("lbl_2", &ids(&["msg_1", "missing"])).expect_err("a missing message must fail the batch");
    assert!(matches!(error, StoreError::NotFound { entity: "message", .. }), "{error}");
    assert!(store.messages_by_label("lbl_2").expect("label view").is_empty(), "the batch was partially applied");
    assert_eq!(store.apply_label("lbl_2", &ids(&["msg_2"])).expect("apply to the good message"), 1);
}

#[test]
fn filters_order_and_paging() {
    let fixture = Fixture::new();
    let store = fixture.open();
    seed(&store);
    store.upsert_message(&sample_message("msg_2", INBOX, "Seconda", "Testo", 1_700_000_200_000)).expect("second message");
    let mut third = sample_message("msg_3", ARCHIVE, "Terza", "Altro testo", 1_700_000_300_000);
    third.summary.unread = false;
    third.summary.starred = true;
    store.upsert_message(&third).expect("third message");
    store
        .create_label(&Label { id: "lbl_1".to_owned(), account_id: Some(ACCOUNT.to_owned()), name: "Lavoro".to_owned(), color: "forest".to_owned() })
        .expect("label");
    store.apply_label("lbl_1", &ids(&["msg_2"])).expect("apply");
    store
        .replace_attachments(
            "msg_2",
            &[Attachment { id: "att_1".to_owned(), filename: "allegato.txt".to_owned(), mime: "text/plain".to_owned(), size: 3, blob_hash: None }],
        )
        .expect("attachment");

    let all = store.list_messages(&MessageFilter::default()).expect("all");
    assert_eq!(all.total, 3);
    assert_eq!(all.messages.iter().map(|message| message.id.as_str()).collect::<Vec<_>>(), vec!["msg_3", "msg_2", "msg_1"]);

    let paged = store.list_messages(&MessageFilter { limit: 1, offset: 1, ..Default::default() }).expect("page");
    assert_eq!((paged.total, paged.messages.len()), (3, 1));
    assert_eq!(paged.messages[0].id, "msg_2");

    let unread = store.list_messages(&MessageFilter { unread: Some(true), ..Default::default() }).expect("unread");
    assert_eq!((unread.total, unread.unread), (2, 2));

    let starred = store.list_messages(&MessageFilter { starred: Some(true), ..Default::default() }).expect("starred");
    assert_eq!(starred.messages.iter().map(|message| message.id.as_str()).collect::<Vec<_>>(), vec!["msg_3"]);

    let attached = store.list_messages(&MessageFilter { has_attachments: Some(true), ..Default::default() }).expect("attachments");
    assert_eq!(attached.messages.iter().map(|message| message.id.as_str()).collect::<Vec<_>>(), vec!["msg_2"]);

    let labelled = store.list_messages(&MessageFilter { label_id: Some("lbl_1".to_owned()), ..Default::default() }).expect("label");
    assert_eq!(labelled.messages.iter().map(|message| message.id.as_str()).collect::<Vec<_>>(), vec!["msg_2"]);

    let inbox = store.list_messages(&MessageFilter { folder_id: Some(INBOX.to_owned()), ..Default::default() }).expect("inbox");
    assert_eq!(inbox.total, 2);
    let archive = store.list_messages(&MessageFilter { folder_id: Some(ARCHIVE.to_owned()), ..Default::default() }).expect("archive");
    assert_eq!(archive.total, 1);

    // Limit 0 is clamped to one row, not "everything".
    let clamped = store.list_messages(&MessageFilter { limit: 0, ..Default::default() }).expect("clamped");
    assert_eq!(clamped.messages.len(), 1);
}

#[test]
fn drafts_revision_increments_and_survives_reopen() {
    let fixture = Fixture::new();
    let store = fixture.open();
    store.upsert_account(&sample_account()).expect("account");

    let draft = DraftUpdate {
        id: "drf_1".to_owned(),
        account_id: ACCOUNT.to_owned(),
        updated_at: 1_700_000_000_000,
        body_text: "Prima bozza".to_owned(),
        to: vec![address("Giulia", "giulia@example.com")],
        cc: Vec::new(),
        bcc: vec![address("Archivio", "archivio@example.com")],
        subject: "Bozza".to_owned(),
        in_reply_to: Some("<msg_1@example.com>".to_owned()),
    };
    let first = store.save_draft(&draft).expect("save");
    assert_eq!(first.revision, 1);
    let second =
        store.save_draft(&DraftUpdate { body_text: "Seconda bozza".to_owned(), updated_at: 1_700_000_100_000, ..draft.clone() }).expect("save again");
    assert_eq!(second.revision, 2);
    assert_eq!(second.body_text, "Seconda bozza");
    assert_eq!(second.bcc, vec![address("Archivio", "archivio@example.com")]);
    assert_eq!(store.list_drafts(Some(ACCOUNT)).expect("drafts").len(), 1);
    drop(store);

    let store = fixture.open();
    let reloaded = store.get_draft("drf_1").expect("get").expect("the draft survived reopen");
    assert_eq!(reloaded.revision, 2);
    assert_eq!(reloaded.subject, "Bozza");
    assert!(store.delete_draft("drf_1").expect("delete"));
    assert!(!store.delete_draft("drf_1").expect("delete again"));
    assert!(store.get_draft("drf_1").expect("get").is_none());
}

#[test]
fn save_draft_message_is_one_transaction_and_validates_attachment_ids() {
    let fixture = Fixture::new();
    let store = fixture.open();
    seed(&store);
    store.upsert_folder(&folder("drafts", "Bozze", FolderKind::Drafts)).expect("drafts");
    store.upsert_folder(&folder("sent", "Inviati", FolderKind::Sent)).expect("sent");

    let mut first = sample_message("drf_1", "drafts", "Bozza A", "Prima versione", 1_700_000_200_000);
    first.summary.unread = false;
    let first_attachment =
        Attachment { id: "att_a".to_owned(), filename: "a.pdf".to_owned(), mime: "application/pdf".to_owned(), size: 10, blob_hash: None };
    store.save_draft_message(&first, std::slice::from_ref(&first_attachment)).expect("first draft");

    let mut second = sample_message("drf_2", "drafts", "Bozza B", "Seconda", 1_700_000_200_100);
    second.summary.unread = false;
    let second_attachment = Attachment { id: "att_b".to_owned(), filename: "b.pdf".to_owned(), ..first_attachment.clone() };
    store.save_draft_message(&second, std::slice::from_ref(&second_attachment)).expect("second draft");

    let before = store.get_draft("drf_1").expect("draft row").expect("drf_1");
    assert_eq!(before.revision, 1);
    assert_eq!(store.attachments("drf_1").expect("attachments"), vec![first_attachment.clone()]);

    let mut conflict = first.clone();
    conflict.body_text = "Non deve essere scritto".to_owned();
    let error = store
        .save_draft_message(&conflict, std::slice::from_ref(&second_attachment))
        .expect_err("an attachment owned by another message must be refused");
    assert!(matches!(error, StoreError::AttachmentOwnedByOther { ref id, ref owner } if id == "att_b" && owner == "drf_2"), "{error}");

    let after = store.get_draft("drf_1").expect("draft row").expect("drf_1");
    assert_eq!(after.revision, before.revision, "the failed save bumped the revision");
    assert_eq!(after.body_text, "Prima versione", "the failed save changed the draft row");
    assert_eq!(store.get_message("drf_1").expect("message").expect("drf_1").body_text, "Prima versione", "the failed save changed the display row");
    assert_eq!(store.attachments("drf_1").expect("attachments"), vec![first_attachment.clone()], "the failed save changed attachments");

    let error = store
        .save_draft_message(&first, &[first_attachment.clone(), first_attachment.clone()])
        .expect_err("a repeated attachment id must be refused");
    assert!(matches!(error, StoreError::DuplicateAttachment { ref id } if id == "att_a"), "{error}");
    assert_eq!(store.get_draft("drf_1").expect("draft row").expect("drf_1").revision, before.revision);
}

#[test]
fn send_keeps_the_draft_revision_sequence() {
    let fixture = Fixture::new();
    let store = fixture.open();
    seed(&store);
    store.upsert_folder(&folder("drafts", "Bozze", FolderKind::Drafts)).expect("drafts");
    store.upsert_folder(&folder("sent", "Inviati", FolderKind::Sent)).expect("sent");

    let mut message = sample_message("drf_1", "drafts", "Bozza", "Prima", 1_700_000_200_000);
    message.summary.unread = false;
    assert_eq!(store.save_draft_message(&message, &[]).expect("save v1").revision, 1);
    assert_eq!(store.save_draft_message(&message, &[]).expect("save v2").revision, 2);

    let mut sent = message.clone();
    sent.summary.folder_id = "sent".to_owned();
    sent.summary.date = 1_700_000_300_000;
    store.send_draft_message(&sent, None).expect("send");
    assert!(store.get_draft("drf_1").expect("draft row").is_none());
    assert_eq!(store.get_message("drf_1").expect("message").expect("sent message").summary.folder_id, "sent");

    store.move_messages(&["drf_1".to_owned()], "drafts").expect("move back");
    assert_eq!(store.save_draft_message(&message, &[]).expect("save v3").revision, 3, "the sequence restarted after a send");
}

#[test]
fn delete_message_removes_the_draft_row_and_sequence() {
    let fixture = Fixture::new();
    let store = fixture.open();
    seed(&store);
    store.upsert_folder(&folder("drafts", "Bozze", FolderKind::Drafts)).expect("drafts");

    let mut message = sample_message("drf_1", "drafts", "Bozza", "Corpo", 1_700_000_200_000);
    message.summary.unread = false;
    assert_eq!(store.save_draft_message(&message, &[]).expect("save").revision, 1);
    assert!(store.delete_message("drf_1").expect("delete"));
    assert!(store.get_draft("drf_1").expect("draft").is_none());
    assert!(store.get_message("drf_1").expect("message").is_none());
    assert!(store.list_orphan_drafts().expect("orphans").is_empty());

    // Reusing the id after a permanent delete starts a fresh sequence, not revision 2.
    store.upsert_message(&message).expect("reinsert display row");
    assert_eq!(store.save_draft_message(&message, &[]).expect("save after delete").revision, 1);
}

#[test]
fn label_sequence_is_a_high_water_mark() {
    let fixture = Fixture::new();
    let store = fixture.open();

    let label = |id: &str, name: &str| Label { id: id.to_owned(), account_id: None, name: name.to_owned(), color: "gold".to_owned() };
    store.create_label(&label("lbl_1", "Uno")).expect("lbl_1");
    assert_eq!(store.get_meta(postino_store::LABEL_SEQ_KEY).expect("meta").as_deref(), Some("1"));
    store.create_label(&label("lbl_5", "Cinque")).expect("lbl_5");
    assert_eq!(store.get_meta(postino_store::LABEL_SEQ_KEY).expect("meta").as_deref(), Some("5"));
    assert!(store.delete_label("lbl_5").expect("delete the highest"));
    assert_eq!(store.get_meta(postino_store::LABEL_SEQ_KEY).expect("meta").as_deref(), Some("5"), "delete lowered the high-water mark");
    store.create_label(&label("lbl_6", "Sei")).expect("lbl_6");
    assert_eq!(store.get_meta(postino_store::LABEL_SEQ_KEY).expect("meta").as_deref(), Some("6"));
}

#[test]
fn reopen_keeps_every_row() {
    let fixture = Fixture::new();
    let store = fixture.open();
    seed(&store);
    store
        .create_label(&Label { id: "lbl_1".to_owned(), account_id: Some(ACCOUNT.to_owned()), name: "Lavoro".to_owned(), color: "forest".to_owned() })
        .expect("label");
    store.apply_label("lbl_1", &ids(&["msg_1"])).expect("apply");
    store
        .replace_attachments(
            "msg_1",
            &[Attachment {
                id: "att_1".to_owned(),
                filename: "rapporto.pdf".to_owned(),
                mime: "application/pdf".to_owned(),
                size: 5,
                blob_hash: None,
            }],
        )
        .expect("attachment");
    store.set_meta("clock", "2026-10-25T10:42:00Z").expect("meta");
    drop(store);

    let store = fixture.open();
    assert_eq!(store.list_accounts().expect("accounts").len(), 1);
    assert_eq!(store.list_folders(Some(ACCOUNT)).expect("folders").len(), 2);
    let message = store.get_message("msg_1").expect("get").expect("message");
    assert_eq!(message.summary.subject, "Rapporto mensile");
    assert_eq!(message.summary.labels, vec!["lbl_1"]);
    assert_eq!(store.attachments("msg_1").expect("attachments").len(), 1);
    assert_eq!(store.counts(Some(INBOX)).expect("counts"), postino_store::MessageCounts { total: 1, unread: 1 });
    assert_eq!(store.get_meta("clock").expect("meta").as_deref(), Some("2026-10-25T10:42:00Z"));
}

#[test]
fn unknown_ids_are_readable_errors() {
    let fixture = Fixture::new();
    let store = fixture.open();
    seed(&store);

    let error = store
        .create_label(&Label {
            id: "lbl_1".to_owned(),
            account_id: Some("missing".to_owned()),
            name: "Lavoro".to_owned(),
            color: "forest".to_owned(),
        })
        .expect_err("an unknown account must be refused");
    assert!(matches!(error, StoreError::NotFound { entity: "account", .. }), "{error}");

    store
        .create_label(&Label { id: "lbl_1".to_owned(), account_id: Some(ACCOUNT.to_owned()), name: "Lavoro".to_owned(), color: "forest".to_owned() })
        .expect("label");
    assert!(matches!(store.update_label("missing", Some("x"), None), Err(StoreError::NotFound { entity: "label", .. })));
    assert!(matches!(store.move_messages(&ids(&["msg_1"]), "missing"), Err(StoreError::NotFound { entity: "folder", .. })));
    let rollback = store.move_messages(&ids(&["msg_1", "missing"]), ARCHIVE).expect_err("a missing message must fail the move batch");
    assert!(matches!(rollback, StoreError::NotFound { entity: "message", .. }), "{rollback}");
    assert_eq!(store.get_message("msg_1").expect("get").expect("message").summary.folder_id, INBOX, "the move batch was partially applied");
    assert!(matches!(store.apply_label("missing", &ids(&["msg_1"])), Err(StoreError::NotFound { entity: "label", .. })));
    assert!(matches!(store.attachments("missing"), Err(StoreError::NotFound { entity: "message", .. })));
    assert!(matches!(store.upsert_message(&sample_message("x", "missing", "s", "b", 0)), Err(StoreError::NotFound { entity: "folder", .. })));

    // A message cannot live in another account's folder, by upsert or by move.
    store.upsert_account(&Account { id: "acc_2".to_owned(), ..sample_account() }).expect("second account");
    store
        .upsert_folder(&Folder { id: "other".to_owned(), account_id: "acc_2".to_owned(), ..folder(ARCHIVE, "Altro", FolderKind::Custom) })
        .expect("other folder");
    let mismatch = store
        .upsert_message(&sample_message("msg_9", "other", "Estranea", "Testo", 0))
        .expect_err("an upsert into another account's folder must be refused");
    assert!(matches!(mismatch, StoreError::AccountMismatch { .. }), "{mismatch}");
    let mismatch = store.move_messages(&ids(&["msg_1"]), "other").expect_err("a cross-account move must be refused");
    assert!(matches!(mismatch, StoreError::AccountMismatch { .. }), "{mismatch}");
    assert_eq!(store.get_message("msg_1").expect("get").expect("message").summary.folder_id, INBOX);

    let duplicate = store
        .create_label(&Label { id: "lbl_2".to_owned(), account_id: Some(ACCOUNT.to_owned()), name: "Lavoro".to_owned(), color: "gold".to_owned() })
        .expect_err("a duplicate label name must be refused");
    assert!(matches!(duplicate, StoreError::Duplicate { entity: "label", .. }), "{duplicate}");
}

#[test]
fn a_wrong_key_cannot_read_repository_rows() {
    let fixture = Fixture::new();
    {
        let store = fixture.open();
        seed(&store);
    }

    let mut other = [0u8; 32];
    getrandom::fill(&mut other).expect("OS random source");
    let error = Store::open(&fixture.path(), &StoreKey::from_bytes(other)).err().expect("a wrong key must not open the store");
    assert!(matches!(error, StoreError::WrongKeyOrCorrupt), "{error}");
}

#[test]
fn no_plaintext_substrings_in_database_or_wal() {
    const SUBJECT: &str = "CANARY-SUBJECT-7b1c9d";
    const BODY: &str = "CANARY-BODY-7b1c9d";
    const FROM: &str = "canary.from@example.com";
    const TO: &str = "canary.to@example.com";
    const LABEL: &str = "CANARY-LABEL-7b1c9d";
    const DRAFT: &str = "CANARY-DRAFT-7b1c9d";
    const ATTACHMENT: &str = "canary-attachment.txt";
    const META: &str = "CANARY-META-7b1c9d";
    let canaries = [SUBJECT, BODY, FROM, TO, LABEL, DRAFT, ATTACHMENT, META];

    let fixture = Fixture::new();
    let store = fixture.open();
    store
        .upsert_account(&Account {
            id: ACCOUNT.to_owned(),
            name: "Canary Studio".to_owned(),
            email: FROM.to_owned(),
            provider: "demo".to_owned(),
            signature: BODY.to_owned(),
            created_at: 1_700_000_000_000,
        })
        .expect("account");
    store.upsert_folder(&folder(INBOX, "Posta in arrivo", FolderKind::Inbox)).expect("folder");
    let mut message = sample_message("msg_1", INBOX, SUBJECT, BODY, 1_700_000_100_000);
    message.summary.to = vec![address("Canary", TO)];
    store.upsert_message(&message).expect("message");
    store
        .create_label(&Label { id: "lbl_1".to_owned(), account_id: Some(ACCOUNT.to_owned()), name: LABEL.to_owned(), color: "forest".to_owned() })
        .expect("label");
    store.apply_label("lbl_1", &ids(&["msg_1"])).expect("apply");
    store
        .replace_attachments(
            "msg_1",
            &[Attachment { id: "att_1".to_owned(), filename: ATTACHMENT.to_owned(), mime: "text/plain".to_owned(), size: 5, blob_hash: None }],
        )
        .expect("attachment");
    store
        .save_draft(&DraftUpdate {
            id: "drf_1".to_owned(),
            account_id: ACCOUNT.to_owned(),
            updated_at: 1_700_000_100_000,
            body_text: DRAFT.to_owned(),
            to: vec![address("Canary", TO)],
            cc: Vec::new(),
            bcc: Vec::new(),
            subject: SUBJECT.to_owned(),
            in_reply_to: None,
        })
        .expect("draft");
    store.set_meta("canary", META).expect("meta");

    let files = store_files(fixture.dir.path());
    assert!(files.iter().any(|path| path.file_name().is_some_and(|name| name == "store.db")), "the database file is missing: {files:?}");
    assert!(
        files.iter().any(|path| path.file_name().is_some_and(|name| name == "store.db-wal")),
        "the WAL was not live, so the WAL scan proves nothing: {files:?}"
    );
    let db = std::fs::read(fixture.path()).expect("read the database");
    assert_ne!(&db[..16], b"SQLite format 3\0", "the database header is plaintext SQLite");
    for path in &files {
        let bytes = std::fs::read(path).expect("read a store file");
        assert!(!contains(&bytes, fixture.key.as_bytes()), "the store key is present in {}", path.display());
    }
    assert_no_canary(&files, &canaries, "while the WAL is live");

    drop(store);
    assert_no_canary(&store_files(fixture.dir.path()), &canaries, "after the connection closed");

    let bytes = files.iter().filter_map(|path| std::fs::metadata(path).ok().map(|meta| meta.len())).sum::<u64>();
    println!("no-plaintext: files={} bytes={bytes} canaries={} all absent", files.len(), canaries.len());
}

fn store_files(dir: &Path) -> Vec<PathBuf> {
    let mut files = std::fs::read_dir(dir)
        .expect("read the store directory")
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.is_file())
        .collect::<Vec<_>>();
    files.sort();
    files
}

fn assert_no_canary(files: &[PathBuf], canaries: &[&str], when: &str) {
    for path in files {
        let bytes = std::fs::read(path).expect("read a store file");
        for canary in canaries {
            assert!(!contains(&bytes, canary.as_bytes()), "plaintext {canary:?} leaked into {} {when}", path.display());
        }
    }
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|window| window == needle)
}
