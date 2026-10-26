//! P9b thread derivation, the cached conversation rows and saved searches.
//!
//! Every fixture writes through the repository, so the assertions cover the same transaction path a
//! sync uses: the message row, its references, the thread assignment and the cache row.

use std::path::PathBuf;

use postino_store::{
    Account, Address, FlagUpdate, Folder, FolderKind, Message, MessageFilter, MessageSummary, SavedSearch, Store, StoreKey, ThreadFilter,
};

const ACCOUNT: &str = "acc_thr";
const INBOX: &str = "fld_thr_inbox";
const ARCHIVE: &str = "fld_thr_archive";
const T0: i64 = 1_700_000_000_000;

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
        self.dir.path().join("threads.db")
    }

    fn open(&self) -> Store {
        Store::open(&self.path(), &self.key).expect("open the store")
    }
}

fn seed(store: &Store) {
    store
        .upsert_account(&Account {
            id: ACCOUNT.to_owned(),
            name: "Studio Verdi".to_owned(),
            email: "studio@example.com".to_owned(),
            provider: "custom".to_owned(),
            signature: String::new(),
            created_at: T0,
        })
        .expect("account");
    for (id, name, kind) in [(INBOX, "INBOX", FolderKind::Inbox), (ARCHIVE, "Archive", FolderKind::Archive)] {
        store
            .upsert_folder(&Folder {
                id: id.to_owned(),
                account_id: ACCOUNT.to_owned(),
                name: name.to_owned(),
                kind,
                unread: 0,
                total: 0,
                uid_validity: Some(1),
                uid_next: Some(1),
                last_sync_at: None,
            })
            .expect("folder");
    }
}

fn message(id: &str, folder: &str, message_id: Option<&str>, subject: &str, date: i64) -> Message {
    Message {
        summary: MessageSummary {
            id: id.to_owned(),
            account_id: ACCOUNT.to_owned(),
            folder_id: folder.to_owned(),
            thread_id: format!("provisional_{id}"),
            from: Address::new("Giulia", "giulia@example.com"),
            to: vec![Address::new("Me", "me@example.com")],
            subject: subject.to_owned(),
            snippet: format!("snippet {id}"),
            date,
            unread: true,
            starred: false,
            has_attachments: false,
            labels: Vec::new(),
        },
        uid: Some(1),
        message_id: message_id.map(str::to_owned),
        cc: Vec::new(),
        bcc: Vec::new(),
        body_text: format!("body {id}"),
        body_html: None,
        remote_images: 0,
        size: Some(64),
    }
}

fn insert(store: &Store, row: &Message, parent: Option<&str>, references: &[&str]) {
    let refs = references.iter().map(|reference| (*reference).to_owned()).collect::<Vec<_>>();
    store.upsert_synced_message_with_refs(row, &[], parent, &refs).expect("message");
}

fn thread_of(store: &Store, id: &str) -> String {
    store.get_message(id).expect("read").expect("message").summary.thread_id
}

fn thread_row(store: &Store, id: &str) -> Option<postino_store::ThreadSummary> {
    store
        .list_threads(&ThreadFilter { account_id: Some(ACCOUNT.to_owned()), limit: 200, ..ThreadFilter::default() })
        .expect("threads")
        .threads
        .into_iter()
        .find(|thread| thread.id == id)
}

#[test]
fn root_reply_and_missing_parent_become_one_conversation_and_the_late_parent_joins_it() {
    let fixture = Fixture::new();
    let store = fixture.open();
    seed(&store);

    let root = message("msg_root", INBOX, Some("<root@example.com>"), "Piano editoriale", T0);
    insert(&store, &root, None, &[]);
    assert_eq!(thread_of(&store, "msg_root"), "root@example.com");

    let reply = message("msg_reply", INBOX, Some("<reply@example.com>"), "Re: Piano editoriale", T0 + 1000);
    insert(&store, &reply, Some("root@example.com"), &["root@example.com"]);
    assert_eq!(thread_of(&store, "msg_reply"), "root@example.com");

    let orphan = message("msg_orphan", INBOX, Some("<orphan@example.com>"), "Re: Archivio 2025", T0 + 2000);
    insert(&store, &orphan, None, &["missing@example.com"]);
    assert_eq!(thread_of(&store, "msg_orphan"), "missing@example.com", "an unresolved reference is the phantom root");

    let parent = message("msg_parent", INBOX, Some("<missing@example.com>"), "Archivio 2025", T0 + 3000);
    insert(&store, &parent, None, &[]);
    assert_eq!(thread_of(&store, "msg_parent"), "missing@example.com", "the late parent adopts its phantom container");
    let row = thread_row(&store, "missing@example.com").expect("thread row");
    assert_eq!(row.message_count, 2, "the orphan and its parent are one conversation");
}

#[test]
fn self_references_controls_and_cycles_cannot_loop() {
    let fixture = Fixture::new();
    let store = fixture.open();
    seed(&store);

    let self_ref = message("msg_self", INBOX, Some("<self@example.com>"), "Solo io", T0);
    insert(&store, &self_ref, Some("<self@example.com>"), &["<self@example.com>"]);
    assert_eq!(thread_of(&store, "msg_self"), "self@example.com", "a self-reference is ignored");

    let controlled = message("msg_ctl", INBOX, Some("<ctl@example.com>"), "Controlli", T0 + 1000);
    insert(&store, &controlled, None, &["bad\u{0}id", "<good@example.com>"]);
    assert_eq!(thread_of(&store, "msg_ctl"), "good@example.com", "a control reference is dropped, not substituted");

    let long = format!("<{}@example.com>", "x".repeat(600));
    let capped = message("msg_long", INBOX, Some("<long@example.com>"), "Lungo", T0 + 2000);
    insert(&store, &capped, None, &[long.as_str(), "next@example.com"]);
    assert_eq!(thread_of(&store, "msg_long"), "next@example.com", "a reference over 512 bytes is refused");

    let first = message("msg_a", INBOX, Some("<a@example.com>"), "Ciclo", T0 + 3000);
    insert(&store, &first, None, &["b@example.com"]);
    let second = message("msg_b", INBOX, Some("<b@example.com>"), "Ciclo", T0 + 4000);
    insert(&store, &second, None, &["a@example.com"]);
    assert_eq!(thread_of(&store, "msg_a"), thread_of(&store, "msg_b"), "a two-message cycle converges to one thread");
    let row = thread_row(&store, "b@example.com").expect("thread row");
    assert_eq!(row.message_count, 2);
}

#[test]
fn duplicate_message_ids_resolve_through_the_smallest_local_id() {
    let fixture = Fixture::new();
    let store = fixture.open();
    seed(&store);

    let mut first = message("msg_a_dup", INBOX, Some("<dup@example.com>"), "Copia A", T0);
    first.summary.thread_id = "thread_a".to_owned();
    store.upsert_message(&first).expect("first copy");
    let mut second = message("msg_z_dup", INBOX, Some("<dup@example.com>"), "Copia Z", T0 + 1000);
    second.summary.thread_id = "thread_z".to_owned();
    store.upsert_message(&second).expect("second copy");

    let reply = message("msg_reply_dup", INBOX, Some("<reply-dup@example.com>"), "Re: Copia", T0 + 2000);
    insert(&store, &reply, None, &["dup@example.com"]);
    assert_eq!(thread_of(&store, "msg_reply_dup"), "thread_a", "the smallest local id is the canonical copy");
}

#[test]
fn two_resolved_threads_merge_into_the_smallest_id_and_keep_an_alias() {
    let fixture = Fixture::new();
    let store = fixture.open();
    seed(&store);

    let alpha = message("msg_alpha", INBOX, Some("<alpha@example.com>"), "Alfa", T0);
    insert(&store, &alpha, None, &[]);
    let zulu = message("msg_zulu", INBOX, Some("<zulu@example.com>"), "Zulu", T0 + 1000);
    insert(&store, &zulu, None, &[]);
    let bridge = message("msg_bridge", INBOX, Some("<bridge@example.com>"), "Ponte", T0 + 2000);
    insert(&store, &bridge, None, &["<zulu@example.com>", "<alpha@example.com>"]);

    assert_eq!(thread_of(&store, "msg_bridge"), "alpha@example.com", "the merge target is the smallest id");
    assert_eq!(thread_of(&store, "msg_alpha"), "alpha@example.com");
    assert_eq!(thread_of(&store, "msg_zulu"), "alpha@example.com");
    assert!(thread_row(&store, "zulu@example.com").is_none(), "the merged thread row is deleted");
    let alias = store.canonical_thread(ACCOUNT, "zulu@example.com").expect("alias");
    assert_eq!(alias, "alpha@example.com");
    assert_eq!(store.thread_id_for_message_id(ACCOUNT, "zulu@example.com").expect("lookup").as_deref(), Some("alpha@example.com"));
    let detail = store.get_thread(ACCOUNT, "zulu@example.com", 50, 0).expect("read").expect("detail");
    assert_eq!(detail.thread.id, "alpha@example.com");
    assert_eq!(detail.thread.message_count, 3);
}

#[test]
fn the_same_resolved_threads_merge_the_same_way_in_either_reference_order() {
    let first = Fixture::new();
    let one = first.open();
    seed(&one);
    let alpha = message("msg_alpha", INBOX, Some("<alpha@example.com>"), "Alfa", T0);
    insert(&one, &alpha, None, &[]);
    let zulu = message("msg_zulu", INBOX, Some("<zulu@example.com>"), "Zulu", T0 + 1000);
    insert(&one, &zulu, None, &[]);
    let bridge = message("msg_bridge", INBOX, Some("<bridge@example.com>"), "Ponte", T0 + 2000);
    insert(&one, &bridge, None, &["<alpha@example.com>", "<zulu@example.com>"]);
    let forward = ["msg_alpha", "msg_zulu", "msg_bridge"].map(|id| thread_of(&one, id));

    let second = Fixture::new();
    let two = second.open();
    seed(&two);
    insert(&two, &alpha, None, &[]);
    insert(&two, &zulu, None, &[]);
    insert(&two, &bridge, None, &["<zulu@example.com>", "<alpha@example.com>"]);
    let reverse = ["msg_alpha", "msg_zulu", "msg_bridge"].map(|id| thread_of(&two, id));
    assert_eq!(forward, reverse, "the merge target never depends on the reference order");
    assert_eq!(forward, ["alpha@example.com"; 3]);
}

#[test]
fn an_unresolved_bridge_aliases_every_root_so_a_late_root_joins() {
    let fixture = Fixture::new();
    let store = fixture.open();
    seed(&store);

    let bridge = message("msg_bridge", INBOX, Some("<bridge@example.com>"), "Ponte", T0);
    insert(&store, &bridge, None, &["<zulu@example.com>", "<alpha@example.com>"]);
    assert_eq!(thread_of(&store, "msg_bridge"), "zulu@example.com", "the first valid reference names the thread");
    let alpha = message("msg_alpha", INBOX, Some("<alpha@example.com>"), "Alfa", T0 + 1000);
    insert(&store, &alpha, None, &[]);
    assert_eq!(thread_of(&store, "msg_alpha"), "zulu@example.com", "the late root joins the conversation it was cited in");
    assert_eq!(store.canonical_thread(ACCOUNT, "alpha@example.com").expect("alias"), "zulu@example.com");
    let row = thread_row(&store, "zulu@example.com").expect("thread row");
    assert_eq!(row.message_count, 2);
}

#[test]
fn the_subject_fallback_is_narrow_and_never_merges_two_threads() {
    let fixture = Fixture::new();
    let store = fixture.open();
    seed(&store);

    let root = message("msg_project", INBOX, Some("<project@example.com>"), "Progetto Alfa", T0);
    insert(&store, &root, None, &[]);
    let reply = message("msg_project_reply", INBOX, Some("<project-reply@example.com>"), "Re: Progetto Alfa", T0 + 1000);
    insert(&store, &reply, None, &[]);
    assert_eq!(thread_of(&store, "msg_project_reply"), "project@example.com", "one subject match attaches the reply");

    let short = message("msg_short", INBOX, Some("<short@example.com>"), "Re: aaa", T0 + 2000);
    insert(&store, &short, None, &[]);
    assert_eq!(thread_of(&store, "msg_short"), "short@example.com", "a normalised subject under 8 bytes never attaches");

    let no_prefix = message("msg_no_prefix", INBOX, Some("<no-prefix@example.com>"), "Progetto Alfa", T0 + 3000);
    insert(&store, &no_prefix, None, &[]);
    assert_eq!(thread_of(&store, "msg_no_prefix"), "no-prefix@example.com", "no reply prefix means no fallback");

    let other = message("msg_other_project", INBOX, Some("<other-project@example.com>"), "Progetto Alfa", T0 + 4000);
    insert(&store, &other, None, &[]);
    let ambiguous = message("msg_ambiguous", INBOX, Some("<ambiguous@example.com>"), "Re: Progetto Alfa", T0 + 5000);
    insert(&store, &ambiguous, None, &[]);
    assert_eq!(thread_of(&store, "msg_ambiguous"), "ambiguous@example.com", "two matching threads drop the fallback");

    // A re-fetch of the first fallback reply must not move it now that a second thread has the same
    // subject: the row already had a thread and no references, so it keeps it.
    insert(&store, &reply, None, &[]);
    assert_eq!(thread_of(&store, "msg_project_reply"), "project@example.com", "a fallback attachment survives a re-fetch");
    let project = thread_row(&store, "project@example.com").expect("project thread");
    assert_eq!(project.message_count, 2);

    let old = message("msg_old", INBOX, Some("<old@example.com>"), "Vecchio", T0);
    insert(&store, &old, None, &[]);
    let late = message("msg_late", INBOX, Some("<late@example.com>"), "Re: Vecchio", T0 + 20 * 24 * 60 * 60 * 1000);
    insert(&store, &late, None, &[]);
    assert_eq!(thread_of(&store, "msg_late"), "late@example.com", "a match over 14 days away is not attached");
}

#[test]
fn a_resync_reopen_and_uidvalidity_reset_keep_the_thread_id() {
    let fixture = Fixture::new();
    let store = fixture.open();
    seed(&store);

    let root = message("msg_root", INBOX, Some("<root@example.com>"), "Radice", T0);
    insert(&store, &root, None, &[]);
    let reply = message("msg_reply", INBOX, Some("<reply@example.com>"), "Re: Radice", T0 + 1000);
    insert(&store, &reply, Some("root@example.com"), &["<root@example.com>"]);
    assert_eq!(thread_of(&store, "msg_reply"), "root@example.com");

    insert(&store, &reply, Some("root@example.com"), &["<root@example.com>"]);
    assert_eq!(thread_of(&store, "msg_reply"), "root@example.com", "a re-fetch of the same row is idempotent");
    let row = thread_row(&store, "root@example.com").expect("thread row");
    assert_eq!(row.message_count, 2, "a re-fetch must not duplicate a thread message");

    let reopened = fixture.open();
    assert_eq!(thread_of(&reopened, "msg_reply"), "root@example.com");

    reopened.reset_folder_messages(INBOX).expect("UIDVALIDITY reset");
    assert!(thread_row(&reopened, "root@example.com").is_none(), "the emptied cache row is removed");
    insert(&reopened, &root, None, &[]);
    insert(&reopened, &reply, Some("root@example.com"), &["<root@example.com>"]);
    assert_eq!(thread_of(&reopened, "msg_root"), "root@example.com");
    assert_eq!(thread_of(&reopened, "msg_reply"), "root@example.com", "a reset and re-download re-derive the same id");
}

#[test]
fn counts_are_mailbox_wide_while_views_include_threads_with_a_message_in_scope() {
    let fixture = Fixture::new();
    let store = fixture.open();
    seed(&store);

    let root = message("msg_root", INBOX, Some("<root@example.com>"), "Report", T0);
    insert(&store, &root, None, &[]);
    let mut reply = message("msg_reply", ARCHIVE, Some("<reply@example.com>"), "Re: Report", T0 + 1000);
    reply.summary.starred = true;
    insert(&store, &reply, Some("root@example.com"), &["<root@example.com>"]);
    store.set_flags(&["msg_root".to_owned()], FlagUpdate { unread: Some(false), starred: Some(false) }).expect("read the root");

    let archived = store
        .list_threads(&ThreadFilter {
            account_id: Some(ACCOUNT.to_owned()),
            folder_id: Some(ARCHIVE.to_owned()),
            limit: 50,
            ..ThreadFilter::default()
        })
        .expect("archive view");
    assert_eq!(archived.threads.len(), 1, "the thread is in the archive scope");
    let row = &archived.threads[0];
    assert_eq!(row.message_count, 2, "the count includes the inbox message");
    assert_eq!(row.unread_count, 1);
    assert_eq!(row.starred_count, 1);
    assert_eq!(archived.total, 1);
    assert_eq!(archived.unread_threads, 1);
    assert_eq!(archived.unread_messages, 1);

    let unread = store
        .list_threads(&ThreadFilter { account_id: Some(ACCOUNT.to_owned()), unread: Some(true), limit: 50, ..ThreadFilter::default() })
        .expect("unread view");
    assert_eq!(unread.threads.len(), 1);
    let read_only = store
        .list_threads(&ThreadFilter { account_id: Some(ACCOUNT.to_owned()), unread: Some(false), limit: 50, ..ThreadFilter::default() })
        .expect("read view");
    assert_eq!(read_only.threads.len(), 1, "the root is read but its thread still has an unread message in scope");
    assert_eq!(read_only.threads[0].unread_count, 1);

    let page = store.list_messages(&MessageFilter { folder_id: Some(ARCHIVE.to_owned()), ..MessageFilter::default() }).expect("messages");
    assert_eq!(page.messages[0].thread_id, "root@example.com");
}

#[test]
fn moving_a_thread_member_across_folders_keeps_the_cache_and_lists_the_thread() {
    let fixture = Fixture::new();
    let store = fixture.open();
    seed(&store);
    let root = message("msg_root", INBOX, Some("<root@example.com>"), "Trasloco", T0);
    insert(&store, &root, None, &[]);
    let reply = message("msg_reply", INBOX, Some("<reply@example.com>"), "Re: Trasloco", T0 + 1000);
    insert(&store, &reply, Some("root@example.com"), &["<root@example.com>"]);

    store.move_messages(&["msg_reply".to_owned()], ARCHIVE).expect("move");
    assert_eq!(thread_of(&store, "msg_reply"), "root@example.com", "a move does not change the thread");
    let row = thread_row(&store, "root@example.com").expect("thread row");
    assert_eq!(row.message_count, 2);
    assert_eq!(row.latest_message_id.as_deref(), Some("msg_reply"));

    for folder in [INBOX, ARCHIVE] {
        let page = store
            .list_threads(&ThreadFilter {
                account_id: Some(ACCOUNT.to_owned()),
                folder_id: Some(folder.to_owned()),
                limit: 50,
                ..ThreadFilter::default()
            })
            .expect("folder view");
        assert_eq!(page.threads.len(), 1, "{folder} must list the thread");
    }
}

#[test]
fn deleting_the_last_message_removes_the_thread_and_flags_refresh_its_counts() {
    let fixture = Fixture::new();
    let store = fixture.open();
    seed(&store);
    let root = message("msg_root", INBOX, Some("<root@example.com>"), "Cancellazione", T0);
    insert(&store, &root, None, &[]);
    let reply = message("msg_reply", INBOX, Some("<reply@example.com>"), "Re: Cancellazione", T0 + 1000);
    insert(&store, &reply, Some("root@example.com"), &["<root@example.com>"]);

    store.set_flags(&["msg_root".to_owned(), "msg_reply".to_owned()], FlagUpdate { unread: Some(false), starred: None }).expect("read");
    let row = thread_row(&store, "root@example.com").expect("thread row");
    assert_eq!(row.unread_count, 0);
    assert_eq!(row.state, "read");

    store.delete_message("msg_reply").expect("delete one");
    let row = thread_row(&store, "root@example.com").expect("thread row");
    assert_eq!(row.message_count, 1);
    store.delete_message("msg_root").expect("delete the last");
    assert!(thread_row(&store, "root@example.com").is_none(), "an empty thread row is removed");
}

#[test]
fn thread_lookups_use_the_derived_indexes() {
    let fixture = Fixture::new();
    let path = fixture.path();
    let store = fixture.open();
    seed(&store);
    drop(store);
    let connection = rusqlite::Connection::open(&path).expect("raw open");
    let hex = fixture.key.as_bytes().iter().map(|byte| format!("{byte:02x}")).collect::<String>();
    connection.execute_batch(&format!("PRAGMA key = \"x'{hex}'\";")).expect("raw key");
    let plans = connection
        .prepare("EXPLAIN QUERY PLAN SELECT thread_id FROM messages WHERE account_id = ?1 AND message_id IN (?2, ?3) ORDER BY id ASC LIMIT 1")
        .expect("plan")
        .query_map(["acc_thr", "a@example.com", "<a@example.com>"], |row| row.get::<_, String>(3))
        .expect("rows")
        .collect::<rusqlite::Result<Vec<_>>>()
        .expect("plan rows")
        .join(" ");
    assert!(plans.contains("messages_account_message_id"), "the reference lookup is not indexed: {plans}");
    for index in ["messages_account_message_id", "messages_account_subject_norm_date", "thread_aliases_account_new"] {
        let exists: bool = connection
            .query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'index' AND name = ?1)", [index], |row| row.get(0))
            .expect("index lookup");
        assert!(exists, "the v7 index {index} is missing");
    }
}

#[test]
fn sending_a_draft_refreshes_the_old_draft_thread_and_leaves_no_ghost() {
    let fixture = Fixture::new();
    let store = fixture.open();
    seed(&store);
    let mut draft = message("draft_1", INBOX, None, "Bozza", T0);
    draft.summary.thread_id = "thr_draft_1".to_owned();
    draft.uid = None;
    store.save_draft_message(&draft, &[]).expect("save draft");
    assert!(thread_row(&store, "thr_draft_1").is_some());

    let mut sent = draft.clone();
    sent.summary.folder_id = ARCHIVE.to_owned();
    sent.summary.thread_id = "sent-root@example.com".to_owned();
    sent.uid = None;
    sent.message_id = Some("<sent-root@example.com>".to_owned());
    store.send_draft_message(&sent, None).expect("send");

    assert!(thread_row(&store, "thr_draft_1").is_none(), "the old draft thread is a ghost");
    let row = thread_row(&store, "sent-root@example.com").expect("sent thread");
    assert_eq!(row.message_count, 1);
    assert_eq!(row.latest_message_id.as_deref(), Some("draft_1"));
    assert!(store.get_thread(ACCOUNT, "thr_draft_1", 50, 0).expect("old id").is_none());
    let threads = store.list_threads(&ThreadFilter { account_id: Some(ACCOUNT.to_owned()), limit: 200, ..ThreadFilter::default() }).expect("threads");
    assert_eq!(threads.threads.len(), 1, "one sent message must be one conversation: {:?}", threads.threads);
}

#[test]
fn saved_searches_are_unique_ignoring_case_and_recent_queries_are_bounded() {
    let fixture = Fixture::new();
    let store = fixture.open();
    seed(&store);
    let saved = SavedSearch {
        id: "saved_1".to_owned(),
        account_id: ACCOUNT.to_owned(),
        name: "Fatture".to_owned(),
        query: "has:attachment".to_owned(),
        sort: "relevance".to_owned(),
        created_at: T0,
        updated_at: T0,
    };
    store.create_saved_search(&saved).expect("create");
    let duplicate = SavedSearch { id: "saved_2".to_owned(), name: "fatture".to_owned(), ..saved.clone() };
    assert!(store.create_saved_search(&duplicate).is_err(), "names are unique ignoring case");

    let updated = store.update_saved_search("saved_1", Some("Fatture 2026"), Some("is:unread"), Some("date")).expect("update");
    assert_eq!(updated.name, "Fatture 2026");
    assert_eq!(updated.query, "is:unread");
    assert_eq!(updated.sort, "date");
    assert_eq!(store.count_saved_searches(ACCOUNT).expect("count"), 1);
    assert_eq!(store.list_saved_searches(Some(ACCOUNT)).expect("list").len(), 1);

    let recent = ["uno", "due", "tre", "quattro", "cinque", "sei", "sette", "otto", "nove"];
    let bounded = recent.iter().take(8).map(|query| (*query).to_owned()).collect::<Vec<_>>();
    store.set_recent_searches(ACCOUNT, &bounded).expect("recent");
    assert_eq!(store.recent_searches(ACCOUNT).expect("read"), bounded);
    assert!(store.clear_recent_searches(ACCOUNT).expect("clear"));
    assert!(store.recent_searches(ACCOUNT).expect("read").is_empty());

    assert!(store.delete_saved_search("saved_1").expect("delete"));
    assert!(store.list_saved_searches(Some(ACCOUNT)).expect("list").is_empty());
    let plaintext = std::fs::read(fixture.path()).expect("file");
    assert!(plaintext.windows(7).all(|window| window != b"Fatture"), "saved names must stay inside SQLCipher");
}
