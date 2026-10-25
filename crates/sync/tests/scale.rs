//! Scale: a 2 000-message sync in CI, and 10 000 behind `POSTINO_SLOW_TESTS=1`.
//!
//! The budget is deliberately generous: it catches a quadratic pass or a per-message full-folder
//! scan, not a slow machine. `crates/sync/README.md` records the measured time and peak memory.

mod support;

use std::time::{Duration, Instant};

use postino_testkit::{TestFolder, TestMessage, synthetic_message};
use support::{Harness, RunningServer, seeded, sync};

const TWO_THOUSAND_BUDGET: Duration = Duration::from_secs(180);
const TEN_THOUSAND_BUDGET: Duration = Duration::from_secs(1800);

fn mailbox(count: u32) -> Vec<TestFolder> {
    let messages = (1..=count).map(|uid| TestMessage::synthetic(synthetic_message(uid))).collect::<Vec<_>>();
    vec![TestFolder::new("INBOX", 1).with_special_use("\\Inbox").with_messages(messages)]
}

fn run_scale(count: u32, budget: Duration) {
    let harness = Harness::new();
    let store = seeded(&harness);
    let server = RunningServer::start(mailbox(count));
    let started = Instant::now();
    let outcome = sync(&store, &server).expect("sync");
    let elapsed = started.elapsed();
    server.finish();

    assert_eq!(outcome.fetched, count);
    assert_eq!(store.list_all_messages().expect("messages").len(), usize::try_from(count).expect("count"));
    let inbox = support::folder(&store, "INBOX");
    assert_eq!(store.counts(Some(&inbox.id)).expect("counts").total, count);
    assert_eq!(store.get_folder(&inbox.id).expect("folder").expect("row").total, count);
    assert_eq!(store.get_sync_state(&inbox.id).expect("state").expect("row").uid_next, count.saturating_add(1));
    support::assert_consistent(&store);
    println!("synced {count} messages in {elapsed:?}");
    assert!(elapsed < budget, "{count} messages took {elapsed:?}, over the {budget:?} budget");
}

#[test]
fn two_thousand_messages() {
    run_scale(2_000, TWO_THOUSAND_BUDGET);
}

#[test]
#[ignore = "slow: set POSTINO_SLOW_TESTS=1 and run cargo test -p postino-sync --test scale -- --ignored ten_thousand_messages"]
fn ten_thousand_messages() {
    assert_eq!(
        std::env::var("POSTINO_SLOW_TESTS").as_deref(),
        Ok("1"),
        "the ignored 10 000-message test needs POSTINO_SLOW_TESTS=1; see crates/sync/README.md"
    );
    run_scale(10_000, TEN_THOUSAND_BUDGET);
}
