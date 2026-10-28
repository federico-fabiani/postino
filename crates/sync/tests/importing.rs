//! Local `.eml` import: a reused Message-ID must duplicate, never relocate cached mail.

mod support;

use postino_store::{Address, Folder, FolderKind, Message, MessageSummary};
use postino_sync::{SyncLimits, import_message};
use postino_testkit::{MailboxScript, TestFolder, TestMessage, synthetic_message};
use support::{ACCOUNT_ID, Harness, RunningServer, account, message_by_header, seeded, sync};

fn one_inbox(message: TestMessage) -> Vec<TestFolder> {
    vec![TestFolder::new("INBOX", 10).with_special_use("\\Inbox").with_messages(vec![message])]
}

#[test]
fn importing_a_copy_never_moves_the_synced_original_to_the_import_folder() {
    let harness = Harness::new();
    let store = seeded(&harness);
    let synthetic = synthetic_message(1);
    let raw = synthetic.raw.clone();
    let server = RunningServer::start_with(one_inbox(TestMessage::synthetic(synthetic)), MailboxScript::default());
    sync(&store, &server).expect("initial sync");
    server.finish();

    let original = message_by_header(&store, "synthetic-1@example.test");
    let original_folder = original.summary.folder_id.clone();
    let import = Folder {
        id: "fld_import".to_owned(),
        account_id: ACCOUNT_ID.to_owned(),
        name: "Import".to_owned(),
        kind: FolderKind::Custom,
        unread: 0,
        total: 0,
        uid_validity: None,
        uid_next: None,
        last_sync_at: None,
    };
    store.upsert_folder(&import).expect("import folder");

    let imported = import_message(&store, &account(), &import, &raw, &SyncLimits::default()).expect("import");
    let row = store.get_message(&imported.message_id).expect("read imported").expect("imported row");
    assert_eq!(row.summary.folder_id, "fld_import");
    assert_eq!(row.summary.subject, "Synthetic message 1");
    assert!(row.body_text.contains("number 1"));

    let untouched = store.get_message(&original.summary.id).expect("read original").expect("original row");
    assert_eq!(untouched.summary.folder_id, original_folder, "importing a copy moved the cached original");
    assert_eq!(store.list_all_messages().expect("all messages").len(), 2);
}

#[test]
fn importing_never_adopts_a_uid_less_sent_copy_with_the_same_message_id() {
    let harness = Harness::new();
    let store = seeded(&harness);
    let sent = Folder {
        id: "fld_local_sent".to_owned(),
        account_id: ACCOUNT_ID.to_owned(),
        name: "Sent".to_owned(),
        kind: FolderKind::Sent,
        unread: 0,
        total: 0,
        uid_validity: None,
        uid_next: None,
        last_sync_at: None,
    };
    store.upsert_folder(&sent).expect("sent folder");
    let message_id = "synthetic-1@example.test".to_owned();
    store
        .upsert_message(&Message {
            summary: MessageSummary {
                id: "msg_local_sent".to_owned(),
                account_id: ACCOUNT_ID.to_owned(),
                folder_id: sent.id.clone(),
                thread_id: "msg_local_sent".to_owned(),
                from: Address::new("Studio Verdi", "studio@example.com"),
                to: vec![Address::new("Cliente", "cliente@example.com")],
                subject: "Synthetic message 1".to_owned(),
                snippet: "sent copy".to_owned(),
                date: 1_700_000_000_000,
                unread: false,
                starred: false,
                has_attachments: false,
                labels: Vec::new(),
            },
            uid: None,
            message_id: Some(message_id),
            cc: Vec::new(),
            bcc: Vec::new(),
            body_text: "sent copy".to_owned(),
            body_html: None,
            remote_images: 0,
            size: None,
        })
        .expect("local sent row");
    let import = Folder {
        id: "fld_import".to_owned(),
        account_id: ACCOUNT_ID.to_owned(),
        name: "Import".to_owned(),
        kind: FolderKind::Custom,
        unread: 0,
        total: 0,
        uid_validity: None,
        uid_next: None,
        last_sync_at: None,
    };
    store.upsert_folder(&import).expect("import folder");

    let raw = synthetic_message(1).raw;
    let imported = import_message(&store, &account(), &import, &raw, &SyncLimits::default()).expect("import");
    assert_ne!(imported.message_id, "msg_local_sent");
    let untouched = store.get_message("msg_local_sent").expect("read sent").expect("sent row");
    assert_eq!(untouched.summary.folder_id, sent.id, "the import moved a uid-less Sent copy");
    let copy = store.get_message(&imported.message_id).expect("read copy").expect("import row");
    assert_eq!(copy.summary.folder_id, import.id);
    assert_eq!(store.list_all_messages().expect("all messages").len(), 2);
}
