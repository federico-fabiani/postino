//! P12a: Gmail OAuth (XOAUTH2) end to end through the engine, with a scripted token endpoint.
//!
//! The fake IMAP server asserts the exact XOAUTH2 payload and never offers PLAIN, so an accidental
//! password fallback fails the test instead of passing silently. The fake token endpoint asserts the
//! refresh grant and records every request.

mod support;

use std::time::{Duration, Instant};

use postino_core::{AccountSetup, CredentialVault, Engine, OAuthAccess, OAuthGrant, OpError, Provider};
use postino_testkit::{FakeOAuth, SMTP_USER, TestFolder, TlsFixture, TokenReply, expected_xoauth2, token_json};
use support::{Harness, MemoryVault, RunningServer, RunningSmtp};

const ACCESS: &str = "ya29.access-token";
const ROTATED: &str = "1//rotated-refresh-token";
const CLIENT_ID: &str = "client-id.apps.googleusercontent.com";
const CLIENT_SECRET: &str = "client-secret-value";
const REFRESH: &str = "1//stored-refresh-token";

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().expect("test runtime")
}

fn folders() -> Vec<TestFolder> {
    vec![TestFolder::new("INBOX", 10).with_special_use("\\Inbox")]
}

fn oauth_setup() -> AccountSetup {
    AccountSetup {
        provider: Provider::Gmail,
        email: postino_testkit::USER.to_owned(),
        name: Some("Postino Test".to_owned()),
        password: zeroize::Zeroizing::new(String::new()),
        oauth: Some(OAuthGrant::new(CLIENT_ID, CLIENT_SECRET, REFRESH)),
        oauth_access: None,
        host: None,
        port: None,
        tls: None,
        smtp: None,
        extra_roots: Vec::new(),
    }
}

#[test]
fn an_oauth_account_refreshes_once_and_speaks_xoauth2_for_imap_and_smtp() {
    let runtime = runtime();
    let tls = TlsFixture::generate();
    let imap = RunningServer::start_oauth(folders(), 2, ACCESS);
    let smtp = RunningSmtp::start(1, |_, mut conn| async move {
        conn.greet(&["AUTH XOAUTH2", "SIZE 52428800"]).await;
        conn.auth_xoauth2(&expected_xoauth2(SMTP_USER, ACCESS)).await;
    });
    let oauth = runtime
        .block_on(async { FakeOAuth::start(&tls, vec![TokenReply::json(token_json(ACCESS, Some(ROTATED), 3600)), TokenReply::json("{}")]).await });

    let harness = Harness::new();
    let vault = MemoryVault::new();
    let mut engine = Engine::open_with_vault(harness.open(), vault.clone()).expect("engine");
    engine.set_test_extra_roots(vec![imap.ca(), smtp.ca(), tls.ca()]);
    engine.set_test_oauth_hosts(("localhost".to_owned(), imap.port()), Some(("localhost".to_owned(), smtp.port())));
    engine.set_test_oauth_endpoints("https://localhost/authorize".to_owned(), oauth.token_endpoint(), oauth.revoke_endpoint());

    let account = engine.add_account(oauth_setup()).expect("the OAuth account is added");
    let grant = vault.load_oauth_grant(&account.account.id).expect("keyring").expect("a grant");
    assert_eq!(grant.refresh_token(), ROTATED, "a rotated refresh token replaces the one in the grant");
    assert!(!vault.contains(&account.account.id), "no app password is stored next to a grant");

    let report = engine.sync_now(Some(account.account.id.as_str())).expect("sync works with XOAUTH2");
    assert_eq!(report.account, account.account.id);
    assert_eq!(oauth.requests().len(), 1, "the access token is reused while it is fresh, not refreshed per connection");

    let removed = engine.remove_account(&account.account.id, true).expect("remove");
    assert!(removed.removed);
    assert!(removed.warnings.is_empty(), "{:?}", removed.warnings);
    assert!(vault.load_oauth_grant(&account.account.id).expect("keyring").is_none(), "the grant is deleted");
    let requests = oauth.requests();
    assert_eq!(requests.len(), 2, "account_remove revokes the grant");
    assert!(requests[1].contains("token="), "the revocation posts the refresh token as a form field");
    assert!(requests[1].contains("1%2F%2Frotated"), "the rotated token is the one revoked");

    imap.finish();
    smtp.finish();
    runtime.block_on(oauth.finish());
}

#[test]
fn the_code_exchange_token_probes_without_a_second_token_request() {
    let imap = RunningServer::start_oauth(folders(), 2, ACCESS);
    let smtp = RunningSmtp::start(1, |_, mut conn| async move {
        conn.greet(&["AUTH XOAUTH2", "SIZE 52428800"]).await;
        conn.auth_xoauth2(&expected_xoauth2(SMTP_USER, ACCESS)).await;
    });

    let harness = Harness::new();
    let vault = MemoryVault::new();
    let mut engine = Engine::open_with_vault(harness.open(), vault.clone()).expect("engine");
    engine.set_test_extra_roots(vec![imap.ca(), smtp.ca()]);
    engine.set_test_oauth_hosts(("localhost".to_owned(), imap.port()), Some(("localhost".to_owned(), smtp.port())));

    let mut setup = oauth_setup();
    setup.oauth_access = Some(OAuthAccess::new(ACCESS, i64::MAX));
    let account = engine.add_account(setup).expect("the exchange token is used for the probe");
    let grant = vault.load_oauth_grant(&account.account.id).expect("keyring").expect("a grant");
    assert_eq!(grant.refresh_token(), REFRESH, "no refresh happened, so the refresh token is not rotated");
    let report = engine.sync_now(Some(account.account.id.as_str())).expect("sync reuses the cached exchange token");
    assert_eq!(report.account, account.account.id);
    assert_eq!(vault.len(), 1, "only the grant is stored");

    imap.finish();
    smtp.finish();
}

#[test]
fn a_stale_code_exchange_token_is_refreshed_before_the_probe() {
    let runtime = runtime();
    let tls = TlsFixture::generate();
    let imap = RunningServer::start_oauth(folders(), 1, ACCESS);
    let smtp = RunningSmtp::start(1, |_, mut conn| async move {
        conn.greet(&["AUTH XOAUTH2", "SIZE 52428800"]).await;
        conn.auth_xoauth2(&expected_xoauth2(SMTP_USER, ACCESS)).await;
    });
    let oauth = runtime.block_on(async { FakeOAuth::start(&tls, vec![TokenReply::json(token_json(ACCESS, Some(ROTATED), 3600))]).await });

    let harness = Harness::new();
    let vault = MemoryVault::new();
    let mut engine = Engine::open_with_vault(harness.open(), vault.clone()).expect("engine");
    engine.set_test_extra_roots(vec![imap.ca(), smtp.ca(), tls.ca()]);
    engine.set_test_oauth_hosts(("localhost".to_owned(), imap.port()), Some(("localhost".to_owned(), smtp.port())));
    engine.set_test_oauth_endpoints("https://localhost/authorize".to_owned(), oauth.token_endpoint(), oauth.revoke_endpoint());

    let mut setup = oauth_setup();
    setup.oauth_access = Some(OAuthAccess::new("ya29.expired", 0));
    let account = engine.add_account(setup).expect("a stale hint refreshes instead of failing");
    assert_eq!(oauth.requests().len(), 1, "exactly one refresh when the hint is expired");
    assert_eq!(vault.load_oauth_grant(&account.account.id).expect("keyring").expect("grant").refresh_token(), ROTATED);

    imap.finish();
    smtp.finish();
    runtime.block_on(oauth.finish());
}

#[test]
fn removing_an_account_succeeds_when_google_cannot_be_reached_and_a_retry_is_a_no_op() {
    let imap = RunningServer::start_oauth(folders(), 1, ACCESS);
    let smtp = RunningSmtp::start(1, |_, mut conn| async move {
        conn.greet(&["AUTH XOAUTH2", "SIZE 52428800"]).await;
        conn.auth_xoauth2(&expected_xoauth2(SMTP_USER, ACCESS)).await;
    });
    let harness = Harness::new();
    let vault = MemoryVault::new();
    let mut engine = Engine::open_with_vault(harness.open(), vault.clone()).expect("engine");
    engine.set_test_extra_roots(vec![imap.ca(), smtp.ca()]);
    engine.set_test_oauth_hosts(("localhost".to_owned(), imap.port()), Some(("localhost".to_owned(), smtp.port())));
    engine.set_test_oauth_endpoints(
        "https://localhost/authorize".to_owned(),
        "https://127.0.0.1:1/token".to_owned(),
        "https://127.0.0.1:1/revoke".to_owned(),
    );
    let mut setup = oauth_setup();
    setup.oauth_access = Some(OAuthAccess::new(ACCESS, i64::MAX));
    let account = engine.add_account(setup).expect("add with the exchange token");

    let removed = engine.remove_account(&account.account.id, true).expect("a dead token endpoint must not fail removal");
    assert!(removed.removed);
    assert_eq!(removed.warnings.len(), 1, "{:?}", removed.warnings);
    assert!(removed.warnings[0].contains("could not be revoked"), "{:?}", removed.warnings);
    assert!(removed.warnings[0].contains("Google account security page"), "{:?}", removed.warnings);
    assert!(vault.load_oauth_grant(&account.account.id).expect("keyring").is_none(), "the grant is deleted locally");
    assert!(harness.open().list_accounts().expect("accounts").is_empty(), "the local account is gone");

    let retry = engine.remove_account(&account.account.id, true).expect("a retry after success is not unknown account");
    assert!(!retry.removed);
    assert!(retry.warnings.is_empty());

    imap.finish();
    smtp.finish();
}

#[test]
fn a_keyring_read_failure_after_removal_is_a_warning_not_an_error() {
    let imap = RunningServer::start_oauth(folders(), 1, ACCESS);
    let smtp = RunningSmtp::start(1, |_, mut conn| async move {
        conn.greet(&["AUTH XOAUTH2", "SIZE 52428800"]).await;
        conn.auth_xoauth2(&expected_xoauth2(SMTP_USER, ACCESS)).await;
    });
    let harness = Harness::new();
    let vault = MemoryVault::new();
    let mut engine = Engine::open_with_vault(harness.open(), vault.clone()).expect("engine");
    engine.set_test_extra_roots(vec![imap.ca(), smtp.ca()]);
    engine.set_test_oauth_hosts(("localhost".to_owned(), imap.port()), Some(("localhost".to_owned(), smtp.port())));
    let mut setup = oauth_setup();
    setup.oauth_access = Some(OAuthAccess::new(ACCESS, i64::MAX));
    let account = engine.add_account(setup).expect("add");
    vault.fail_loads();

    let removed = engine.remove_account(&account.account.id, true).expect("a keyring read failure must not fail removal");
    assert!(removed.removed);
    assert_eq!(removed.warnings.len(), 1, "{:?}", removed.warnings);
    assert!(removed.warnings[0].contains("could not be read"), "{:?}", removed.warnings);
    assert!(harness.open().list_accounts().expect("accounts").is_empty());

    imap.finish();
    smtp.finish();
}

#[test]
fn the_account_secrets_never_reach_any_store_file() {
    let imap = RunningServer::start_oauth(folders(), 1, ACCESS);
    let smtp = RunningSmtp::start(1, |_, mut conn| async move {
        conn.greet(&["AUTH XOAUTH2", "SIZE 52428800"]).await;
        conn.auth_xoauth2(&expected_xoauth2(SMTP_USER, ACCESS)).await;
    });
    let harness = Harness::new();
    let vault = MemoryVault::new();
    let mut engine = Engine::open_with_vault(harness.open(), vault.clone()).expect("engine");
    engine.set_test_extra_roots(vec![imap.ca(), smtp.ca()]);
    engine.set_test_oauth_hosts(("localhost".to_owned(), imap.port()), Some(("localhost".to_owned(), smtp.port())));
    let mut setup = oauth_setup();
    setup.oauth_access = Some(OAuthAccess::new(ACCESS, i64::MAX));
    engine.add_account(setup).expect("add");

    let root = harness.path().parent().expect("store directory").to_path_buf();
    let mut files = 0;
    for entry in std::fs::read_dir(root).expect("read store directory") {
        let path = entry.expect("entry").path();
        if !path.is_file() {
            continue;
        }
        files += 1;
        let bytes = std::fs::read(&path).expect("read store file");
        for secret in [CLIENT_SECRET, REFRESH, ACCESS, ROTATED] {
            assert!(!bytes.windows(secret.len()).any(|window| window == secret.as_bytes()), "{} leaked into {}", secret, path.display());
        }
    }
    assert!(files >= 1, "the store wrote at least one file");

    imap.finish();
    smtp.finish();
}

#[test]
fn a_stalled_revocation_is_bounded_to_fifteen_seconds_and_never_fails_removal() {
    // Accept the TLS connection and never answer: the removal waits for its bounded revoke budget,
    // reports a warning, and still leaves no local account behind.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("stalled revoke listener");
    let port = listener.local_addr().expect("addr").port();
    let stalled = std::thread::spawn(move || {
        if let Ok((_stream, _)) = listener.accept() {
            std::thread::sleep(Duration::from_secs(20));
        }
    });
    let imap = RunningServer::start_oauth(folders(), 1, ACCESS);
    let smtp = RunningSmtp::start(1, |_, mut conn| async move {
        conn.greet(&["AUTH XOAUTH2", "SIZE 52428800"]).await;
        conn.auth_xoauth2(&expected_xoauth2(SMTP_USER, ACCESS)).await;
    });
    let harness = Harness::new();
    let vault = MemoryVault::new();
    let mut engine = Engine::open_with_vault(harness.open(), vault).expect("engine");
    engine.set_test_extra_roots(vec![imap.ca(), smtp.ca()]);
    engine.set_test_oauth_hosts(("localhost".to_owned(), imap.port()), Some(("localhost".to_owned(), smtp.port())));
    engine.set_test_oauth_endpoints(
        "https://localhost/authorize".to_owned(),
        "https://127.0.0.1:1/token".to_owned(),
        format!("https://localhost:{port}/revoke"),
    );
    let mut setup = oauth_setup();
    setup.oauth_access = Some(OAuthAccess::new(ACCESS, i64::MAX));
    let account = engine.add_account(setup).expect("add");

    let started = Instant::now();
    let removed = engine.remove_account(&account.account.id, true).expect("a stalled revoke is a warning");
    let elapsed = started.elapsed();
    assert!(removed.removed);
    assert!(elapsed >= Duration::from_secs(14), "the revoke budget is 15s, not a hang: {elapsed:?}");
    assert!(elapsed < Duration::from_secs(20), "the revoke budget bounded the wait: {elapsed:?}");
    assert_eq!(removed.warnings.len(), 1, "{:?}", removed.warnings);
    assert!(removed.warnings[0].contains("could not be revoked"), "{:?}", removed.warnings);

    imap.finish();
    smtp.finish();
    stalled.join().expect("stalled listener");
}

#[test]
fn a_revoked_grant_fails_with_a_sentence_and_writes_nothing() {
    let runtime = runtime();
    let tls = TlsFixture::generate();
    let oauth = runtime.block_on(async { FakeOAuth::start(&tls, vec![TokenReply::error(400, "invalid_grant")]).await });

    let harness = Harness::new();
    let vault = MemoryVault::new();
    let mut engine = Engine::open_with_vault(harness.open(), vault.clone()).expect("engine");
    engine.set_test_extra_roots(vec![tls.ca()]);
    engine.set_test_oauth_hosts(("127.0.0.1".to_owned(), 1), None);
    engine.set_test_oauth_endpoints("https://localhost/authorize".to_owned(), oauth.token_endpoint(), oauth.revoke_endpoint());

    let error = engine.add_account(oauth_setup()).expect_err("the grant is dead");
    let sentence = error.to_string();
    assert!(matches!(error, OpError::Auth(_)), "{sentence}");
    assert!(sentence.contains("authorization"), "{sentence}");
    for secret in [CLIENT_SECRET, REFRESH, ACCESS] {
        assert!(!sentence.contains(secret), "the error leaked a secret: {sentence}");
    }
    assert!(harness.open().list_accounts().expect("accounts").is_empty(), "nothing was written");
    assert_eq!(vault.len(), 0, "nothing was written to the keyring");

    runtime.block_on(oauth.finish());
}

#[test]
fn an_app_password_account_keeps_the_old_path_and_has_no_grant() {
    let harness = Harness::new();
    let vault = MemoryVault::new();
    let server = RunningServer::start(folders(), 1);
    let mut engine = Engine::open_with_vault(harness.open(), vault.clone()).expect("engine");
    engine.set_test_extra_roots(vec![server.ca()]);

    let account = engine.add_account(support::account_setup(&server)).expect("password account");
    assert!(vault.contains(&account.account.id), "the IMAP password is stored as before");
    assert!(vault.load_oauth_grant(&account.account.id).expect("keyring").is_none(), "no grant is invented");

    server.finish();
}

#[test]
fn an_oauth_account_refuses_an_app_password_and_vice_versa() {
    let harness = Harness::new();
    let vault = MemoryVault::new();
    let mut engine = Engine::open_with_vault(harness.open(), vault).expect("engine");

    let mut both = oauth_setup();
    both.password = zeroize::Zeroizing::new("an-app-password".to_owned());
    let error = engine.add_account(both).expect_err("both credentials are refused");
    assert!(matches!(error, OpError::InvalidArgs(_)), "{error}");
    assert!(error.to_string().contains("never both"), "{error}");

    let mut custom = oauth_setup();
    custom.provider = Provider::Custom;
    custom.host = Some("localhost".to_owned());
    custom.port = Some(1);
    custom.tls = Some(postino_core::MailTls::Implicit);
    let error = engine.add_account(custom).expect_err("only Gmail has an OAuth provider");
    assert!(error.to_string().contains("app password"), "{error}");

    assert!(engine.accounts().is_empty());
}
