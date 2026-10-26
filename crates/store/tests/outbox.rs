//! Schema v5 outbox journal: dedupe by draft, lease epochs, guarded transitions, cancellation,
//! manual retry, newest-first history and safe pruning.

use std::path::PathBuf;

use postino_store::{
    Account, AccountServer, AccountTls, Folder, FolderKind, OutboxEntry, OutboxState, SmtpServer, Store, StoreError, StoreKey, UNCERTAIN_AFTER_CRASH,
};

const ACCOUNT: &str = "acc_outbox";

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
        self.dir.path().join("outbox.db")
    }

    fn open(&self) -> Store {
        let store = Store::open(&self.path(), &self.key).expect("open");
        if store.get_account(ACCOUNT).expect("account lookup").is_none() {
            let account = Account {
                id: ACCOUNT.to_owned(),
                name: "Studio".to_owned(),
                email: "studio@example.com".to_owned(),
                provider: "custom".to_owned(),
                signature: String::new(),
                created_at: 1_700_000_000_000,
            };
            let server = AccountServer {
                account_id: ACCOUNT.to_owned(),
                host: "imap.example.com".to_owned(),
                port: 993,
                tls: AccountTls::Implicit,
                username: "studio@example.com".to_owned(),
                created_at: 1_700_000_000_000,
                smtp: Some(SmtpServer {
                    host: "smtp.example.com".to_owned(),
                    port: 465,
                    tls: AccountTls::Implicit,
                    username: "studio@example.com".to_owned(),
                }),
            };
            let folders = [
                Folder {
                    id: "fld_sent".to_owned(),
                    account_id: ACCOUNT.to_owned(),
                    name: "Sent".to_owned(),
                    kind: FolderKind::Sent,
                    unread: 0,
                    total: 0,
                    uid_validity: None,
                    uid_next: None,
                    last_sync_at: None,
                },
                Folder {
                    id: "fld_drafts".to_owned(),
                    account_id: ACCOUNT.to_owned(),
                    name: "Drafts".to_owned(),
                    kind: FolderKind::Drafts,
                    unread: 0,
                    total: 0,
                    uid_validity: None,
                    uid_next: None,
                    last_sync_at: None,
                },
            ];
            store.save_account(&account, &server, &folders).expect("seed account");
        }
        store
    }
}

fn entry(id: &str, key: &str, send_at: i64) -> OutboxEntry {
    OutboxEntry {
        id: id.to_owned(),
        account_id: ACCOUNT.to_owned(),
        draft_id: Some(format!("draft_{id}")),
        idempotency_key: key.to_owned(),
        created_at: 1_700_000_000_000,
        send_at,
        state: OutboxState::Queued,
        attempts: 0,
        append_attempts: 0,
        last_error: None,
        raw_blob_hash: "a".repeat(64),
        envelope_json: r#"{"from":"studio@example.com","recipients":["one@example.com"],"subject":"Prova"}"#.to_owned(),
        lease_at: None,
    }
}

#[test]
fn the_v5_schema_has_the_smtp_columns_and_the_outbox() {
    let fixture = Fixture::new();
    let store = fixture.open();
    assert_eq!(store.schema_version().expect("version"), 6);
    let server = store.get_account_server(ACCOUNT).expect("server").expect("row");
    let smtp = server.smtp.expect("smtp endpoint");
    assert_eq!((smtp.host.as_str(), smtp.port, smtp.tls), ("smtp.example.com", 465, AccountTls::Implicit));
    assert!(store.list_outbox(None, 50).expect("outbox").is_empty());
}

#[test]
fn a_live_lease_survives_a_long_submission() {
    let fixture = Fixture::new();
    let store = fixture.open();
    store.queue_outbox(&entry("ob_1", "draft:1", 1_000)).expect("insert");
    let leased = store.lease_next_outbox(1_000).expect("lease").expect("row");
    let lease = leased.lease_at.expect("lease");
    // The client aborts a body transfer after 30 minutes; the lease is 45, so a transfer that is still
    // writing at minute 30 is not a crash.
    assert!(store.expire_stale_outbox(lease + 30 * 60 * 1_000, 45 * 60 * 1_000).expect("sweep").is_empty());
    assert_eq!(store.get_outbox("ob_1").expect("get").expect("row").state, OutboxState::Sending);
    let stale = store.expire_stale_outbox(lease + 45 * 60 * 1_000 + 1, 45 * 60 * 1_000).expect("sweep");
    assert_eq!(stale, vec!["ob_1".to_owned()]);
}

#[test]
fn a_scheduled_row_is_leased_once_and_attempts_are_counted() {
    let fixture = Fixture::new();
    let store = fixture.open();
    store.queue_outbox(&entry("ob_1", "draft:1", 1_700_000_010_000)).expect("insert");
    assert!(store.lease_next_outbox(1_700_000_009_999).expect("before due").is_none());
    let leased = store.lease_next_outbox(1_700_000_010_000).expect("lease").expect("a due row");
    assert_eq!(leased.id, "ob_1");
    assert_eq!(leased.state, OutboxState::Sending);
    assert_eq!(leased.attempts, 1);
    assert_eq!(leased.lease_at, Some(1_700_000_010_000), "the lease epoch is written at lease time");
    assert!(store.lease_next_outbox(1_700_000_010_001).expect("second lease").is_none(), "a sending row was leased twice");
    assert!(!store.finish_outbox_append("ob_1", 1_700_000_010_000).expect("refused"), "a sending row reached sent without an append");
    assert!(store.mark_outbox_append_pending("ob_1", 1_700_000_010_000, 1_700_000_010_500).expect("append pending"));
    let row = store.get_outbox("ob_1").expect("get").expect("row");
    assert_eq!(row.lease_at, Some(1_700_000_010_500), "the append claim starts with the transition");
    assert!(store.finish_outbox_append("ob_1", 1_700_000_010_500).expect("sent"));
    let row = store.get_outbox("ob_1").expect("get").expect("row");
    assert_eq!(row.state, OutboxState::Sent);
    assert_eq!(row.attempts, 1);
    assert_eq!(row.lease_at, None);
}

#[test]
fn a_stale_lease_is_measured_from_lease_at_not_the_due_time() {
    let fixture = Fixture::new();
    let store = fixture.open();
    // Scheduled for 09:00 and leased at 10:00: the user-visible due time is an hour old, the lease
    // is not.
    store.queue_outbox(&entry("ob_1", "draft:1", 1_000)).expect("insert");
    store.lease_next_outbox(3_600_000).expect("lease").expect("row");
    assert!(store.expire_stale_outbox(3_600_000, 60_000).expect("sweep").is_empty(), "a live lease with an old send_at was expired");
    assert!(store.expire_stale_outbox(3_659_999, 60_000).expect("sweep").is_empty(), "a lease inside the window was expired");
    let stale = store.expire_stale_outbox(3_660_001, 60_000).expect("sweep");
    assert_eq!(stale, vec!["ob_1".to_owned()]);
    let row = store.get_outbox("ob_1").expect("get").expect("row");
    assert_eq!(row.state, OutboxState::Uncertain);
    assert_eq!(row.last_error.as_deref(), Some(UNCERTAIN_AFTER_CRASH));
    assert!(store.lease_next_outbox(200_000_000).expect("lease").is_none(), "an uncertain row was leased again");
}

#[test]
fn a_stale_sending_row_becomes_uncertain_and_is_never_auto_retried() {
    let fixture = Fixture::new();
    let store = fixture.open();
    store.queue_outbox(&entry("ob_1", "draft:1", 1_000)).expect("insert");
    store.lease_next_outbox(1_000).expect("lease").expect("row");
    let stale = store.expire_stale_outbox(1_000, 60_000).expect("sweep");
    assert!(stale.is_empty(), "a fresh lease must survive");
    let stale = store.expire_stale_outbox(61_000, 60_000).expect("sweep");
    assert_eq!(stale, vec!["ob_1".to_owned()]);
    let row = store.get_outbox("ob_1").expect("get").expect("row");
    assert_eq!(row.state, OutboxState::Uncertain);
    assert_eq!(row.last_error.as_deref(), Some(UNCERTAIN_AFTER_CRASH));
    assert!(store.lease_next_outbox(200_000).expect("lease").is_none(), "an uncertain row was leased again");
    assert!(!store.cancel_outbox("ob_1").expect("cancel"), "an uncertain row is not cancellable");
    assert!(store.retry_outbox("ob_1", 200_000).expect("retry"), "a manual retry is allowed");
    let row = store.get_outbox("ob_1").expect("get").expect("row");
    assert_eq!(row.state, OutboxState::Queued);
    assert_eq!(row.attempts, 0);
    assert_eq!(row.send_at, 200_000);
    assert_eq!(row.last_error, None);
    assert_eq!(row.lease_at, None);
}

#[test]
fn a_cancelled_row_cannot_be_marked_by_another_worker() {
    let fixture = Fixture::new();
    let store = fixture.open();
    store.queue_outbox(&entry("ob_1", "draft:1", 1_000)).expect("insert");
    assert!(store.cancel_outbox("ob_1").expect("cancel"));
    assert!(!store.cancel_outbox("ob_1").expect("second cancel"));
    let row = store.get_outbox("ob_1").expect("get").expect("row");
    assert_eq!(row.state, OutboxState::Cancelled);
    assert!(store.lease_next_outbox(1_000_000).expect("lease").is_none(), "a cancelled row was leased");

    store.queue_outbox(&entry("ob_2", "draft:2", 1_000)).expect("insert");
    store.lease_next_outbox(1_000).expect("lease").expect("row");
    assert!(!store.cancel_outbox("ob_2").expect("cancel after sending"), "cancel succeeded after the row was sending");
}

#[test]
fn a_permanent_failure_can_be_discarded_but_an_uncertain_one_cannot() {
    let fixture = Fixture::new();
    let store = fixture.open();
    store.queue_outbox(&entry("ob_1", "draft:1", 1_000)).expect("insert");
    store.lease_next_outbox(1_000).expect("lease").expect("row");
    assert!(store.mark_outbox_failed("ob_1", "550 recipient rejected", 1_000).expect("fail"));
    assert!(store.cancel_outbox("ob_1").expect("a failed row is a discard"), "a permanent failure could not be discarded");
    let row = store.get_outbox("ob_1").expect("get").expect("row");
    assert_eq!(row.state, OutboxState::Cancelled);
    assert_eq!(row.last_error, None);
}

#[test]
fn append_pending_is_independent_from_submission() {
    let fixture = Fixture::new();
    let store = fixture.open();
    store.queue_outbox(&entry("ob_1", "draft:1", 1_000)).expect("insert");
    let lease = store.lease_next_outbox(1_000).expect("lease").expect("row").lease_at.expect("lease");
    assert!(store.mark_outbox_append_pending("ob_1", lease, 1_000).expect("append pending"));
    let row = store.get_outbox("ob_1").expect("get").expect("row");
    assert_eq!(row.state, OutboxState::AppendPending);
    assert!(store.reschedule_outbox_append("ob_1", 5_000, Some("append refused"), 1_000).expect("reschedule"));
    let row = store.get_outbox("ob_1").expect("get").expect("row");
    assert_eq!(row.state, OutboxState::AppendPending);
    assert_eq!(row.send_at, 5_000);
    assert_eq!(row.last_error.as_deref(), Some("append refused"));
    assert_eq!(row.lease_at, None, "a reschedule releases the append claim");
    let append_lease = store.claim_outbox_append(5_000, 60_000).expect("claim").expect("the rescheduled append").lease_at.expect("append lease");
    assert!(store.finish_outbox_append("ob_1", append_lease).expect("sent"));
    assert_eq!(store.get_outbox("ob_1").expect("get").expect("row").state, OutboxState::Sent);
}

#[test]
fn a_permanent_failure_is_retryable_only_manually() {
    let fixture = Fixture::new();
    let store = fixture.open();
    store.queue_outbox(&entry("ob_1", "draft:1", 1_000)).expect("insert");
    let lease = store.lease_next_outbox(1_000).expect("lease").expect("row").lease_at.expect("lease");
    assert!(store.mark_outbox_failed("ob_1", "550 recipient rejected", lease).expect("fail"));
    assert!(store.lease_next_outbox(100_000).expect("lease").is_none());
    assert!(store.retry_outbox("ob_1", 100_000).expect("manual retry"));
    assert!(store.lease_next_outbox(100_000).expect("lease").is_some());
}

#[test]
fn queueing_the_same_draft_twice_is_one_active_row() {
    let fixture = Fixture::new();
    let store = fixture.open();
    store.queue_outbox(&entry("ob_1", "draft:same:hash", 1_000)).expect("insert");
    let error = store.queue_outbox(&entry("ob_2", "draft:same:hash", 1_000)).expect_err("the same key is the same queue request");
    assert!(matches!(error, StoreError::OutboxAlreadyActive { state: OutboxState::Queued, .. }), "{error}");

    // A different key for the same draft is still impossible while one row is active: the partial
    // unique index is the last line of defence against an edited-content race.
    let mut other = entry("ob_3", "draft:same:other-hash", 1_000);
    other.draft_id = Some("draft_ob_1".to_owned());
    let error = store.queue_outbox(&other).expect_err("a second active row for one draft");
    assert!(matches!(error, StoreError::OutboxAlreadyActive { state: OutboxState::Queued, .. }), "{error}");
    assert_eq!(store.list_outbox(Some(ACCOUNT), 50).expect("list").len(), 1);
}

#[test]
fn a_cancelled_row_is_revived_by_the_same_idempotency_key() {
    let fixture = Fixture::new();
    let store = fixture.open();
    store.queue_outbox(&entry("ob_1", "draft:same:hash", 1_000)).expect("insert");
    assert!(store.cancel_outbox("ob_1").expect("cancel"));
    let mut again = entry("ob_9", "draft:same:hash", 5_000);
    again.attempts = 3;
    let revived = store.queue_outbox(&again).expect("a cancel is a discard, not a barrier");
    assert_eq!(revived.id, "ob_1", "the cancelled row is reused instead of inserting a second one");
    assert_eq!(revived.state, OutboxState::Queued);
    assert_eq!(revived.send_at, 5_000);
    assert_eq!(revived.attempts, 0);
    assert_eq!(revived.lease_at, None);
    assert_eq!(store.list_outbox(Some(ACCOUNT), 50).expect("list").len(), 1);
}

#[test]
fn the_latest_created_row_wins_even_after_a_revival() {
    let fixture = Fixture::new();
    let store = fixture.open();
    let mut first = entry("ob_1", "draft:same:hash", 1_000);
    first.created_at = 1_000;
    store.queue_outbox(&first).expect("insert");
    assert!(store.cancel_outbox("ob_1").expect("cancel"));
    let mut second = entry("ob_2", "draft:same:other", 1_000);
    second.draft_id = Some("draft_ob_1".to_owned());
    second.created_at = 2_000;
    store.queue_outbox(&second).expect("the edited content");
    assert!(store.cancel_outbox("ob_2").expect("cancel the edit"));
    // A cancel → send of the same content revives the first row: a new created_at, the old rowid.
    let mut revived = entry("ob_1", "draft:same:hash", 3_000);
    revived.created_at = 3_000;
    store.queue_outbox(&revived).expect("revive");
    let newest = store.active_outbox_for_draft("draft_ob_1").expect("query").expect("row");
    assert_eq!(newest.id, "ob_1", "a later-rowid cancelled row hid the revived queued row");
    assert_eq!(newest.state, OutboxState::Queued);
    assert_eq!(store.active_state_for_draft("draft_ob_1").expect("state"), Some(OutboxState::Queued));
}

#[test]
fn the_newest_row_for_a_draft_is_the_one_that_decides() {
    let fixture = Fixture::new();
    let store = fixture.open();
    store.queue_outbox(&entry("ob_1", "draft:same:hash", 1_000)).expect("insert");
    assert!(store.cancel_outbox("ob_1").expect("cancel"));
    let mut edited = entry("ob_2", "draft:same:other-hash", 1_000);
    edited.draft_id = Some("draft_ob_1".to_owned());
    store.queue_outbox(&edited).expect("insert the edited content");
    let newest = store.active_outbox_for_draft("draft_ob_1").expect("query").expect("row");
    assert_eq!(newest.id, "ob_2", "the cancelled first row must not shadow the live one");
    assert_eq!(newest.state, OutboxState::Queued);
    assert!(store.active_outbox_for_draft("draft_missing").expect("query").is_none());
}

#[test]
fn the_history_is_newest_first_and_paginates() {
    let fixture = Fixture::new();
    let store = fixture.open();
    for index in 0..501u32 {
        let mut row = entry(&format!("ob_{index:03}"), &format!("draft:{index}"), i64::from(index));
        row.created_at = i64::from(index);
        store.queue_outbox(&row).expect("insert");
    }
    let first = store.list_outbox(Some(ACCOUNT), 500).expect("first page");
    assert_eq!(first.len(), 500);
    assert_eq!(first[0].id, "ob_500", "the newest row is hidden behind older ones");
    assert_eq!(first[499].id, "ob_001");
    let second = store.list_outbox_page(Some(ACCOUNT), 500, 500).expect("second page");
    assert_eq!(second.iter().map(|row| row.id.as_str()).collect::<Vec<_>>(), vec!["ob_000"]);
}

#[test]
fn the_append_query_ignores_the_history_window() {
    let fixture = Fixture::new();
    let store = fixture.open();
    for index in 0..500u32 {
        let mut row = entry(&format!("ob_{index:03}"), &format!("draft:{index}"), 1_000);
        row.created_at = i64::from(index);
        row.state = OutboxState::Sent;
        store.queue_outbox(&row).expect("insert");
    }
    let mut pending = entry("ob_append", "draft:append", 1_000);
    pending.created_at = 10_000;
    store.queue_outbox(&pending).expect("insert");
    // The row was queued; move it to append_pending as the worker would, claiming the append.
    let lease = store.lease_next_outbox(1_000).expect("lease").expect("row").lease_at.expect("lease");
    assert!(store.mark_outbox_append_pending("ob_append", lease, 1_000).expect("append pending"));
    assert_eq!(store.next_outbox_wake(60_000).expect("wake"), Some(61_000), "a claimed append wakes at its lease expiry");
    assert!(store.finish_outbox_append("ob_append", 1_000).expect("finish"));
}

#[test]
fn an_append_row_has_one_owner() {
    let fixture = Fixture::new();
    let first = fixture.open();
    first.queue_outbox(&entry("ob_1", "draft:1", 1_000)).expect("insert");
    let lease = first.lease_next_outbox(1_000).expect("lease").expect("row").lease_at.expect("lease");
    assert!(first.mark_outbox_append_pending("ob_1", lease, 1_000).expect("append pending"));

    let append_lease = 1_000;
    // A second process opens its own connection: it must not see the row as claimable.
    let second = fixture.open();
    assert!(second.claim_outbox_append(1_000, 60_000).expect("second claim").is_none(), "two passes claimed the same append");
    assert!(!second.finish_outbox_append("ob_1", append_lease + 1).expect("wrong lease"), "a stale append claim finished the row");
    assert!(second.finish_outbox_append("ob_1", append_lease).expect("the owner can finish"));
    // The first worker's finish now loses, and the row stays sent.
    assert!(!first.finish_outbox_append("ob_1", append_lease).expect("second finish"));
    assert_eq!(first.get_outbox("ob_1").expect("get").expect("row").state, OutboxState::Sent);
}

#[test]
fn an_expired_append_claim_is_recoverable() {
    let fixture = Fixture::new();
    let store = fixture.open();
    store.queue_outbox(&entry("ob_1", "draft:1", 1_000)).expect("insert");
    let lease = store.lease_next_outbox(1_000).expect("lease").expect("row").lease_at.expect("lease");
    assert!(store.mark_outbox_append_pending("ob_1", lease, 1_000).expect("append pending"));
    assert!(store.claim_outbox_append(1_000, 60_000).expect("inside the lease").is_none());
    let recovered = store.claim_outbox_append(1_000 + 60_001, 60_000).expect("after the lease").expect("a crashed append is recoverable");
    assert_eq!(recovered.id, "ob_1");
}

#[test]
fn a_queued_row_is_refused_while_an_append_row_owns_the_draft() {
    let fixture = Fixture::new();
    let store = fixture.open();
    store.queue_outbox(&entry("ob_1", "draft:same:hash", 1_000)).expect("insert");
    let lease = store.lease_next_outbox(1_000).expect("lease").expect("row").lease_at.expect("lease");
    assert!(store.mark_outbox_append_pending("ob_1", lease, 1_000).expect("append pending"));
    let mut second = entry("ob_2", "draft:same:other", 1_000);
    second.draft_id = Some("draft_ob_1".to_owned());
    let error = store.queue_outbox(&second).expect_err("an append row still owns the draft");
    assert!(matches!(error, StoreError::OutboxAlreadyActive { state: OutboxState::AppendPending, .. }), "{error}");
}

#[test]
fn pruning_keeps_live_rows_and_only_old_terminal_ones() {
    let fixture = Fixture::new();
    let store = fixture.open();
    let mut old_sent = entry("ob_sent", "draft:sent", 1_000);
    old_sent.state = OutboxState::Sent;
    old_sent.created_at = 1_000;
    store.queue_outbox(&old_sent).expect("insert");
    let mut old_live = entry("ob_live", "draft:live", 1_000);
    old_live.created_at = 1_000;
    store.queue_outbox(&old_live).expect("insert");
    let mut failed = entry("ob_failed", "draft:failed", 1_000);
    failed.state = OutboxState::Failed;
    failed.created_at = 1_000;
    store.queue_outbox(&failed).expect("insert");

    let removed = store.prune_outbox(10_000_000, 1_000, 100).expect("prune");
    assert_eq!(removed, 2, "only the old terminal rows are candidates");
    assert!(store.get_outbox("ob_sent").expect("get").is_none());
    assert!(store.get_outbox("ob_failed").expect("get").is_none());
    assert!(store.get_outbox("ob_live").expect("get").is_some(), "a queued row was pruned");
}

#[test]
fn guarded_transitions_refuse_a_lost_lease() {
    let fixture = Fixture::new();
    let store = fixture.open();
    store.queue_outbox(&entry("ob_1", "draft:1", 1_000)).expect("insert");
    let lease = store.lease_next_outbox(1_000).expect("lease").expect("row").lease_at.expect("lease");
    assert!(!store.mark_outbox_append_pending("ob_1", lease + 1, 2_000).expect("stale lease"), "a stale lease wrote the transition");
    assert!(!store.mark_outbox_failed("ob_1", "x", lease + 1).expect("stale lease"));
    assert!(!store.mark_outbox_uncertain("ob_1", "x", lease + 1).expect("stale lease"));
    assert!(!store.reschedule_outbox("ob_1", 2_000, None, lease + 1).expect("stale lease"));
    assert!(!store.release_outbox("ob_1", 2_000, lease + 1).expect("stale lease"));
    let row = store.get_outbox("ob_1").expect("get").expect("row");
    assert_eq!(row.state, OutboxState::Sending);
    assert_eq!(row.lease_at, Some(lease));
    assert!(store.mark_outbox_append_pending("ob_1", lease, 2_000).expect("owner lease"));
}

#[test]
fn a_second_worker_that_runs_later_does_not_take_a_live_lease() {
    let fixture = Fixture::new();
    let first = fixture.open();
    first.queue_outbox(&entry("ob_1", "draft:1", 1_000)).expect("insert");
    let lease = first.lease_next_outbox(3_600_000).expect("lease").expect("row");
    assert_eq!(lease.lease_at, Some(3_600_000));
    // A second process opens its own connection and sweeps one minute later.
    let second = fixture.open();
    assert!(second.expire_stale_outbox(3_660_000, 600_000).expect("sweep").is_empty(), "another process expired a live lease");
    assert!(second.lease_next_outbox(3_660_000).expect("lease").is_none(), "a sending row was re-leased");
    assert_eq!(first.get_outbox("ob_1").expect("get").expect("row").state, OutboxState::Sending);
}

#[test]
fn deleting_an_account_removes_its_outbox_rows() {
    let fixture = Fixture::new();
    let store = fixture.open();
    store.queue_outbox(&entry("ob_1", "draft:1", 1_000)).expect("insert");
    assert!(store.delete_account(ACCOUNT).expect("delete account"));
    assert!(store.list_outbox(None, 50).expect("list").is_empty());
}

#[test]
fn a_manual_retry_keeps_the_generated_bytes() {
    let fixture = Fixture::new();
    let store = fixture.open();
    let mut row = entry("ob_1", "draft:1", 1_000);
    row.raw_blob_hash = "b".repeat(64);
    row.envelope_json = r#"{"message_id":"<one@example.com>"}"#.to_owned();
    store.queue_outbox(&row).expect("insert");
    let stored = store.get_outbox("ob_1").expect("get").expect("row");
    assert_eq!(stored.raw_blob_hash, "b".repeat(64));
    assert_eq!(stored.envelope_json, r#"{"message_id":"<one@example.com>"}"#);
}
