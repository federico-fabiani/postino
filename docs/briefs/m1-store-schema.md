# Brief — M1 store schema and keyring (P3b)

> **Depends on:** P3a (SQLCipher proof) · **Next:** P3c engine wiring · **No UI work**

## Outcome

The encrypted store becomes the real persistence layer: a platform keyring seam, a schema that can
hold an IMAP mailbox, and a repository whose methods mirror today's operations. Nothing is wired
into the engine or the app yet; P3c does that.

## 1. `postino-platform`

- New foundation crate, no workspace dependencies. `load_or_create_key(scope: &str) ->
  Result<StoreKey, PlatformError>`: 32 bytes from `getrandom`, stored through `keyring-core` +
  `windows-native-keyring-store` (service `postino`, user = scope). Fail closed if the vault is
  unavailable. Never log or Debug the key.
- The store crate must not read the keyring itself.
- Tests use `keyring_core::mock`, never the real vault: create is idempotent, a second load returns
  the same key, a missing vault is a readable error, the key survives a fresh handle.
- Add the layer entry and a README.

## 2. Schema v2 in `postino-store`

Tables (names may change if justified, but the coverage may not):

| Table | Columns |
|---|---|
| `accounts` | id TEXT PK, name, email, provider, signature, created_at INTEGER (unix ms) |
| `folders` | id TEXT PK, account_id FK, name, kind TEXT, unread INTEGER, total INTEGER, uid_validity INTEGER NULL, uid_next INTEGER NULL, last_sync_at INTEGER NULL |
| `messages` | id TEXT PK, account_id FK, folder_id FK, uid INTEGER NULL, message_id TEXT NULL, thread_id TEXT, from_name, from_email, to_json, cc_json, bcc_json, subject, snippet, date INTEGER, unread INTEGER, starred INTEGER, has_attachments INTEGER, body_text, body_html NULL, size INTEGER NULL |
| `attachments` | id TEXT PK, message_id FK ON DELETE CASCADE, filename, mime, size INTEGER, blob_hash TEXT NULL |
| `labels` | id TEXT PK, account_id FK NULL, name, color TEXT, UNIQUE(account_id, name) |
| `message_labels` | message_id FK ON DELETE CASCADE, label_id FK ON DELETE CASCADE, PRIMARY KEY(message_id, label_id) |
| `drafts` | id TEXT PK, account_id FK, revision INTEGER, updated_at INTEGER, body_text, to_json, cc_json, bcc_json, subject, in_reply_to NULL |
| `sync_state` | folder_id TEXT PK, uid_next INTEGER, highest_modseq INTEGER NULL, last_error TEXT NULL |

- Foreign keys ON, WAL, `synchronous = FULL`, 5 s busy timeout (already in P3a).
- Indexes for the hot reads: messages(folder_id, date DESC), messages(account_id, date DESC),
  message_labels(label_id), attachments(message_id), drafts(updated_at).
- Migration is additive from v1; never edit a shipped migration.
- Store dates as unix milliseconds; document the choice.

## 3. Repository API

Plain structs owned by the store crate (no dependency on `postino-core`), mirroring the current
operations so P3c is a mapping exercise:

- accounts: upsert, list, get.
- folders: upsert, list, get, recount (unread/total from messages).
- messages: upsert, get, list(folder or label or all, unread/attachments/starred filters, limit,
  offset), set_flags, move, delete, counts.
- labels: create, update, delete (detach in the same transaction), list with counts, apply/remove
  for a batch (transactional), messages by label.
- attachments: replace for a message, list.
- drafts: save (revision++), get, list, delete.
- meta: get/set (already there).
- Every multi-row mutation is one transaction. Upserts preserve label assignments and attachments
  unless explicitly replaced.

## 4. Tests

- Round-trip an account, folder and message with two recipients, one attachment and two labels.
- Counts match after read/move/delete; deleting a label detaches it and leaves mail intact.
- Label apply/remove idempotent; batch is atomic (a bad message id rolls back all).
- Drafts revision increments and survives reopen.
- Wrong key / schema-too-new errors unchanged.
- File bytes contain no subject, address or body substring.
- Repository reopen after drop keeps every row.

## 5. Docs and gates

- `docs/architecture.md`: schema and repository section, keyring flow, what is still in memory.
- `docs/store-spike.md`: append the P3b outcome.
- README: keep the M1 status honest (store exists, not wired).
- `cargo xtask ci` green; layers updated for `postino-platform` and `postino-store`.

## Out of scope

Engine/app wiring, IMAP, MIME parsing, FTS5, outbox, encrypted blob files for attachment bodies,
daemon. No UI changes.
