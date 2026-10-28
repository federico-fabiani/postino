//! Local `.eml` import: a reused Message-ID must duplicate, never relocate cached mail.

mod support;

use postino_store::{Folder, FolderKind};
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
