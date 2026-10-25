# Brief — P6a accounts, credentials and sync operations

> **Package:** P6a · **Depends on:** P5b (sync), P4b (blobs) · **Next:** P6b onboarding UI and app
> **Owner:** developer agent · **Review:** blind security + engineering
>
> The backend that turns Postino from a demo into a client for a real mailbox. The UI comes in P6b;
> the CLI, MCP and the operation registry must be complete here.

## Outcome

A person can add an Aruba or Gmail account from the CLI with an app password, test it, sync it into
the encrypted store, read the cached mail, and remove it. Passwords live only in the OS keyring. MCP
cannot create or remove an account and cannot see or set a password.

## 1. Credentials in `postino-platform`

- Generalise the keyring seam beyond the store key:
  `store_secret(scope, &[u8])`, `load_secret(scope) -> Option<Zeroizing<Vec<u8>>>`, `delete_secret(scope)`.
- Service `postino`; scope `store:default`, `account:<id>:imap` (SMTP later). Validate scopes like the
  store key; never log, Debug or error with secret bytes.
- Atomic create/update under the same first-run lock discipline; a missing credential is a readable
  error, not a panic. Tests use the mock store and a temp lock path only.

## 2. Schema v4 and account config

Migration v4, additive:

```sql
CREATE TABLE account_servers(
    account_id TEXT PRIMARY KEY REFERENCES accounts(id) ON DELETE CASCADE,
    imap_host TEXT NOT NULL, imap_port INTEGER NOT NULL, imap_tls TEXT NOT NULL,
    imap_username TEXT NOT NULL, created_at INTEGER NOT NULL
);
```

- No password column anywhere. `imap_tls` is `implicit` or `starttls`.
- Repository: upsert/get/delete server config; `delete_account` removes messages, folders, server
  config and drafts for that account in one transaction (labels stay mailbox-wide; attachments
  cascade).
- `Account` gains server fields? Keep the core/store `Account` unchanged; expose a composed
  `AccountView { account, server, sync }` from the engine.

## 3. Autodiscovery and validation

- Known providers, offline table: `aruba` → `imaps.aruba.it:993 implicit`; `gmail` →
  `imap.gmail.com:993 implicit`, with the note that Gmail requires 2FA + app password (or OAuth in
  P12). `custom` requires host, port and TLS.
- Validate the email syntactically, require a non-empty password, reject control characters, cap
  lengths.
- Folder discovery runs through the real `postino-net` client: login, LIST, map special-use. A failed
  login is a readable `Auth` error naming the provider and the likely cause (app password, 2FA,
  wrong host), never echoing the password.

## 4. Operations (registry, CLI, MCP)

| Operation | Args | Notes |
|---|---|---|
| `accounts_list` | `{}` | Extended `AccountView`: provider, email, server host/port/tls, sync state. No secrets. |
| `account_add` | `{ provider, email, name?, password, host?, port?, tls? }` | **sensitive**; tests the connection, stores the credential, writes the account + server row, discovers folders. Returns `AccountView`. |
| `account_test` | `{ id }` | read-only; login + LIST with the stored credential; returns folder count and server capabilities. |
| `account_remove` | `{ id }` | destructive; deletes the credential and the local cache for that account; requires `confirm: true`. |
| `sync_now` | `{ account? }` | write; runs `postino-sync` and returns the outcome. One account if there is one; all if omitted? Require an id when several. |
| `sync_status` | `{ account? }` | read-only; persisted status, last success, error, folders done/total. |

- Add `sensitive: bool` to `OpDef` (default false). The MCP server must not list or dispatch a
  sensitive tool; the CLI can, and passes the password from stdin, never argv. The JSON Schema still
  exists and the description says the password is read from the caller and never logged.
- `read_only`: `accounts_list`, `account_test`, `sync_status`. `destructive`: `account_remove`.
  `sync_now` and `account_add` are writes.
- Errors: auth, TLS, DNS, folder mapping and store failures stay distinct and actionable. An
  `account_add` that fails at login writes **nothing**.

## 5. CLI

```sh
postino account add --provider aruba --email me@example.it --name "Federico"   # prompts, or --password-stdin
postino account test acc_1
postino account list
postino account remove acc_1 --yes
postino sync run [--account acc_1]
postino sync status
```

- Default store: `%LOCALAPPDATA%\Postino\mail.db` (or the platform equivalent) when `--store` is
  omitted and `--demo` is not given. The key comes from the keyring scope `store:default`.
- `--demo` remains the synthetic mailbox and never touches the real store.
- Password prompt must not echo; `--password` as an argv flag does not exist.
- Exit codes: 0 success, 1 operation failure, 2 usage.

## 6. Engine wiring

- `Engine::open` loads accounts, servers, folders, messages and sync status.
- `Engine::add_account(setup)` calls `postino-sync`'s connect/list path (or `postino-net` directly)
  before writing; on success writes account, server, credential and folders; on failure writes
  nothing. It needs a test-only hook for extra TLS roots (no public skip-verification).
- `Engine::sync_now` calls `postino-sync::sync_account`, then reloads the affected account from the
  store into memory (or reopens the engine state) and refreshes counts. A failed sync returns the
  error and leaves the previous cache intact.
- `Engine::remove_account` deletes the credential, then the store rows, then memory; a failed
  keyring delete aborts before the store change (or the reverse? pick the order that cannot lose the
  password while leaving mail behind; document it).
- The demo engine returns readable errors for account/sync operations that need a store.

## 7. Tests

- Platform: secret round trip, update, delete, missing, redaction, mock only.
- Store: migration v4 from v3, cascade delete, server config round trip.
- Engine with the fake IMAP server: `account_add` success (account, server, credential, folders),
  wrong password writes nothing, unreachable host writes nothing, duplicate email rejected.
- `account_test` on a broken credential is a readable Auth error and leaves the account intact.
- `sync_now` on a 3-folder mailbox stores messages and attachments, updates status, reloads memory;
  a second run fetches only new mail; a failed run keeps the cache.
- `account_remove` deletes the credential and the cache, leaves other accounts untouched.
- CLI: add with `--password-stdin`, list, test, sync, status, remove; no password in output; default
  store path only in an isolated temp profile.
- MCP: sensitive tools are absent from `tools/list` and rejected by dispatch; `sync_status` works;
  `sync_now` works; no password in any response.

## 8. Docs

- README: honest status (real accounts, Aruba/Gmail app passwords, sync and cache; Gmail OAuth is
  P12; no SMTP yet).
- `docs/architecture.md`: credential scopes, v4 schema, account lifecycle, sensitive operations.
- `docs/mcp.md`: the sensitive-operation rule and the account/sync tools that remain.
- `docs/threat-model.md`: keyring scopes, no passwords in DB/logs/MCP, `account_remove` semantics.
- ROADMAP: tick onboarding and real-data persistence only when P6b lands; P6a ticks the backend.

## 9. Out of scope

Onboarding screens, app boot on the store, reading-pane rendering, attachment Save UI, SMTP, outbox,
OAuth, autoconfig/SRV beyond the two providers, rules, notifications, daemon.

## Gates

`cargo xtask ci` green, clippy `-D warnings`, no secrets in fixtures/logs, no `unwrap`/`expect`/
`panic!` outside tests. Temp files only in `%TEMP%\opencode\p6a-postino`; do not commit.

## Report

API, schema, operation table with annotations, credential flow, test map, CLI transcripts (password
redacted), and any deviation with the reason.
