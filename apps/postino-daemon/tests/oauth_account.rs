//! P12a: Gmail OAuth reaches the engine through the same IPC operation table as the CLI.
//!
//! The IMAP and SMTP fakes only accept XOAUTH2, so a password fallback would fail. The token endpoint
//! is never contacted: the access token from the code exchange is handed to `account_add`, which is
//! exactly how the CLI and the onboarding screen call it.

mod support;

use std::time::Duration;

use postino_ipc::protocol::method;
use postino_store::Store;
use postino_testkit::{FakeServer, FakeSmtp, TestFolder, TlsFixture, USER, expected_xoauth2, serve_mailbox_with_xoauth2};
use serde_json::{Value, json};
use support::{Fixture, MockVault};

const ACCESS: &str = "ya29.daemon-access";
const REFRESH: &str = "1//daemon-refresh";
const CLIENT_SECRET: &str = "daemon-client-secret";

fn folders() -> Vec<TestFolder> {
    vec![TestFolder::new("INBOX", 10).with_special_use("\\Inbox")]
}

async fn op(client: &mut postino_ipc::client::Client, name: &str, args: Value) -> Value {
    client
        .call(method::OP, json!({ "name": name, "args": args }), Duration::from_secs(15))
        .await
        .unwrap_or_else(|error| panic!("operation {name} failed: {error}"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_oauth_account_is_added_over_ipc_and_removal_is_idempotent() {
    let tls = TlsFixture::generate();
    let imap = FakeServer::start_tls(&tls, move |conn| {
        let folders = folders();
        async move {
            let _ = serve_mailbox_with_xoauth2(conn, folders, postino_testkit::MailboxScript::default(), ACCESS).await;
        }
    })
    .await;
    let smtp = FakeSmtp::start_tls(&tls, |mut conn| async move {
        conn.greet(&["AUTH XOAUTH2", "SIZE 52428800"]).await;
        conn.auth_xoauth2(&expected_xoauth2(USER, ACCESS)).await;
    })
    .await;

    let fixture = Fixture::new();
    let mut config = fixture.config();
    config.extra_roots = vec![tls.ca()];
    config.oauth_hosts = Some((("localhost".to_owned(), imap.port()), Some(("localhost".to_owned(), smtp.port()))));
    config.oauth_endpoints =
        Some(("https://localhost/authorize".to_owned(), "https://127.0.0.1:1/token".to_owned(), "https://127.0.0.1:1/revoke".to_owned()));
    let daemon = postino_daemon::Daemon::spawn(config).expect("daemon");
    let mut client = fixture.client("oauth-add").await;

    let added = op(
        &mut client,
        "account_add",
        json!({
            "provider": "gmail",
            "email": USER,
            "name": "Postino OAuth",
            "password": "",
            "oauth": {
                "client_id": "client-id.apps.googleusercontent.com",
                "client_secret": CLIENT_SECRET,
                "refresh_token": REFRESH,
            },
            "oauth_access": { "token": ACCESS, "expires_at_ms": i64::MAX },
        }),
    )
    .await;
    assert_eq!(added["account"]["account"]["email"], USER);
    let id = added["account"]["account"]["id"].as_str().expect("account id").to_owned();

    let vault = MockVault::default();
    assert!(vault.oauth_grant(&id).is_none(), "a fresh vault has no grant");
    assert!(fixture.vault.oauth_grant(&id).is_some(), "the grant is in the IPC vault");
    assert!(!fixture.vault.has_imap_password(&id), "no app password is stored next to a grant");
    let store = Store::open(&fixture.store_path, &fixture.key).expect("store");
    assert_eq!(store.list_accounts().expect("accounts").len(), 1);

    let removed = op(&mut client, "account_remove", json!({ "id": id, "confirm": true })).await;
    assert_eq!(removed["removed"], true);
    assert_eq!(removed["warnings"].as_array().map(Vec::len), Some(1), "the offline revoke is a warning: {removed}");
    assert!(fixture.vault.oauth_grant(&id).is_none(), "the grant is deleted locally");
    let store = Store::open(&fixture.store_path, &fixture.key).expect("store");
    assert!(store.list_accounts().expect("accounts").is_empty());

    let retry = op(&mut client, "account_remove", json!({ "id": id, "confirm": true })).await;
    assert_eq!(retry["removed"], false, "a retry is a success, not unknown account: {retry}");
    assert_eq!(retry["warnings"].as_array().map(Vec::len), Some(0));

    drop(vault);
    daemon.shutdown().expect("shutdown");
}
