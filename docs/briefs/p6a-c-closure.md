# Brief — P6a-c credential and CLI closure

> **Source:** blind review of `1f39c43` · **Before:** P6b acceptance
> **Rule:** each fix ships with a test that fails without it.

## 1. MAJOR — server text can echo the password

`account_error` formats `SyncError::Connect(source)` and `Protocol` with the server-supplied reason
into `OpError::Network`, which reaches CLI stderr and MCP output. A hostile server that has just
received the password can repeat it (or the SASL payload) in a `NO`/`BAD`/continuation response.

**Fix.** Do not forward server-originated text from `Connect`/`Protocol` to users or agents. Map them
to a fixed, actionable sentence that names the phase and the account/provider, not the raw reason.
`Auth` keeps its provider hint but no server text. If a diagnostic is needed, log only the phase and
the error variant at debug, never the response text. Test with a fake server that answers `LIST`
with `NO <password>` and assert the password string is absent from `OpError`, the CLI output and the
MCP error.

## 2. MINOR — a damaged status blocks account removal

`remove_account` calls `account_view`, which parses `sync_status:<id>`; a malformed value aborts the
removal, exactly when cleanup is needed.

**Fix.** Status is a cache: `account_view`/`accounts_list` return `status: null` with a warning on a
malformed value, and `remove_account` never fails because of it. Test a hand-written invalid status,
then list and remove.

## 3. MINOR — `--password-stdin` echoes at a TTY, and reads before the engine can accept it

**Fix.** Open/validate the engine and the account command first, then read the secret. If
`--password-stdin` is used while stdin is a terminal, refuse with a usage error that says to omit the
flag for the no-echo prompt. Test both: a TTY refusal (simulate with a terminal check seam or a
unit-level helper) and an engine failure before reading (the input must not be consumed).

## 4. MINOR — usage errors exit 1

`OpError::InvalidArgs` (missing `--yes`, invalid email/name/endpoint, malformed `run` JSON, unknown
id) currently maps to 1; README promises 2 for usage. Map `InvalidArgs` → 2, everything else → 1,
clap → 2. Update the exit-code tests to cover each class and the README if the mapping needs a
sentence.

## 5. MINOR — credential rollback on a failed store write is untested

Add a read-only store seam (`Store::open_read_only(path, key)` that checks the schema and refuses
writes) or an equivalent deterministic failure, then test: probe succeeds, credential is stored, the
account transaction fails, the mock vault records the credential deletion, and no account, server or
folder row exists. Removing the cleanup block must fail the test.

## 6. MINOR — two account scopes are not proven independent

Platform test: store different secrets under `account:a:imap` and `account:b:imap`, read both back,
delete one, assert the other and its value survive. This must fail if the scope argument is ignored.

## 7. MINOR — platform docs claim macOS/Linux keyrings that the build does not have

`install_native_store` is Windows-only; every account operation fails elsewhere. State it honestly in
the crate README, root README and architecture: Windows is the only keyring backend in this build;
macOS/Linux land with the packaging milestone. Do not claim a platform-equivalent default store
where the keyring seam cannot open one.

## 8. NIT — read-back comparison drops a cleartext copy

`store_secret` compares a plain `Vec<u8>` read-back. Wrap it in `Zeroizing` like the other paths.

## 9. NIT — `sync_now` can lose the real failure

If reload and sync both fail, the reload error replaces the sync cause. Evaluate the sync error first
and mention the reload failure inside it (or return the sync error and log the reload), so the user
sees the auth/network reason.

## 10. NIT — read-only commands create a store and a keyring entry

`info`, `tools`, `schema` and `account list` with no store create `mail.db` and a `default`
credential. For commands that only read: if the default store file does not exist, report “no
account configured” without creating it or touching the keyring; `--store` on a missing path is a
usage/read error, not a silent creation. Mutations (`account add`, `sync`) may create it. Test the
read path with a temp profile and assert no files and no vault calls.

## Gates

`cargo xtask ci` green, clippy `-D warnings`, no password in any error/output/log, tests use the mock
vault and temp paths only. Temp files only in `%TEMP%\opencode\p6ac-postino`; do not commit.

## Report

Per finding: fixed/rejected, the failing-before evidence, and any CLI output that changed.
