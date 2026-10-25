# Brief — P3d persistence correctness

> **Source:** blind store/engine audit of HEAD `63701a6` · **Depends on:** P3c · **Before:** P5
> **Rule:** every fix ships with a test that fails without it.

## Why now

The audit found a blocker and two majors that make the store lie about what it persisted. P5 will
write real mail through this layer; fixing it afterwards would mean migrating damaged stores.

## Findings and decisions

### F1 — BLOCKER: `save_draft` is three transactions (accept)
`engine.rs:432-438` upserts the message, replaces attachments, then writes the revision. A failure at
step 2 or 3 leaves a ghost draft or a half-updated one, while the error says nothing changed.

**Fix.** One `postino-store` method that performs the whole draft save in a single
`BEGIN IMMEDIATE` transaction: upsert message, replace attachments, bump/insert the revision row,
refresh the drafts folder counts. The engine calls it once. On error nothing is persisted and memory
is untouched.

**Also.** `attachments.id` is a global primary key and is caller-supplied. Validate before the
write: ids unique within the request; an id owned by another message is a clear error telling the
caller to omit it so the engine assigns one; empty/blank ids are assigned as today.

### F2 — MAJOR: partial `draft_save` wipes omitted fields (accept)
A body-only update clears recipients, subject, cc/bcc and attachments because the request fields
default to empty.

**Fix at the operation boundary.** `draft_save` becomes patch-on-update:
- every content field is `Option<_>` in `DraftSaveArgs`; absent = keep the current value, present
  (even empty/`[]`) = replace it;
- create (`id` absent) treats absent fields as empty;
- the operation description states the rule in one sentence an agent can act on;
- the CLI sends only the flags the user actually passed, and adds `--account`;
- the app keeps sending full content (its autosave owns every field) and needs no behaviour change.

`Engine::save_draft` keeps full-replace semantics; the merge lives in `ops.rs` (or one helper).

### F3 — MAJOR (latent): drafts and sends ignore the message's account (accept)
`save_draft` uses `accounts.first()`, `send_draft` uses the first Sent folder, and `store_map`
drops the row's `account_id`, so a second account's draft can be moved into the first account's
Sent.

**Fix.**
- `send_draft` resolves the Sent folder **of the draft's own account** (via the draft's folder) and
  errors if that account has none. It must never pick another account's folder.
- `draft_save` on update keeps the draft's account and folder. On create, `account` may be given;
  with exactly one account it is implied; with several and no `account`, return an actionable
  error instead of guessing.
- Add an internal check that the target folder belongs to the account the row is written with.
- Tests with two accounts: create for B, send from B, assert B's Sent and account id, and that A is
  untouched.

### F4 — MINOR: deleting a message resurrects its draft (accept)
`delete_message` leaves the `drafts` row; `Engine::open` materialises it again.

**Fix.** Delete the draft row (and revision counter) in the same transaction as the message, and
test delete → reopen → gone. Document the cascade.

### F5 — MINOR: label ids are reused after a reopen (accept)
`next_label_seq` is derived from surviving rows, so deleting the highest id lowers the mark.

**Fix.** Persist the high-water mark in `meta` (`label_seq`) and bump it inside the label-create
transaction. `Engine::open` seeds from `max(meta, existing ids)` and repairs the meta value if an
older store lacks it. Test create → delete highest → reopen → create, assert a fresh id; demo and
store agree.

### F6 — MINOR: draft revisions can go backwards (accept)
`send_draft` deletes the revision row; moving the message back to Drafts and saving restarts at 1.

**Fix.** Migration v3 adds `draft_seq(message_id TEXT PRIMARY KEY, revision INTEGER NOT NULL)`.
The atomic save reads and increments it; `send_draft` keeps it; deleting a draft/message removes
it. Test save v1, v2 → send → move back → save v3, assert revision 3.

### F7 — MINOR: orphan drafts are memory-only and un-mutable (accept)
A `drafts` row without a message row is loaded but not repaired, so flags/labels fail with a
misleading error and store counts disagree.

**Fix.** During `Engine::open`, repair each orphan draft into a real message row (Drafts folder of
its account, body and recipients from the revision row) in one transaction before hydrating, then
recount. If repair fails, fail the open with an actionable error rather than serve a ghost.

### F8 — MINOR: the zeroising pragma reallocates and leaks the key in freed heap (accept)
`String::with_capacity(80)` for an 83-byte value; the old buffer is freed unzeroised, and the loop
copies the array to the stack.

**Fix.** Size the buffer exactly (or with headroom), iterate by reference, zeroise on every path,
and add a test asserting `capacity == len` after construction and that `Debug` of the key type stays
redacted. Update the README claim only if the guarantee still differs from SQLCipher's internal copy.

### F9 — MINOR: every store failure is reported as a disk problem (accept)
Constraint violations, unknown ids and account mismatches all say "check free space", and the text
claims nothing changed even when it did (F1).

**Fix.** Give `StoreError` a category (`Invalid`, `NotFound`, `Conflict`, `Unavailable`, `Corrupt`)
or match on its variants in `store_map`, and emit:
- `OpError::InvalidArgs` for caller mistakes (duplicate attachment id, account mismatch, unknown
  folder/account) with a specific sentence;
- `OpError::Failed` for I/O, wrong key, corrupt data, schema-too-new, with the storage advice only
  there.
No message may claim "nothing changed" unless the transaction guarantees it. Test each category
through a real engine call, not by formatting the error directly.

### F10 — MINOR: `draft_send` is labelled demo while persisting (accept)
The title says "Send a draft (demo)", the CLI prints "(demo)" unconditionally, and with `--store`
the move is durable.

**Fix.** Title: "Move a draft to Sent (local)". Description: no SMTP is opened in this build; the
move is local and durable on a store. CLI and UI print the same truth in both modes. Keep a clear
"Arriva con M2" note in the UI where it already exists.

### F11 — MINOR: tests cannot catch F1/F3/F11-class defects (accept)
Add tests that inspect the **store** after a failure and after a reopen:
- failed draft save (injected duplicate attachment id, and a closed/bad connection if practical)
  leaves the message row, attachments and revision exactly as before;
- partial update preserves omitted fields, and an explicit empty value clears them;
- two-account create/send routing;
- revision monotonic across send + undo;
- delete → reopen → gone;
- label id across reopen;
- pragma capacity;
- error categories;
- keyring first-run race (F12).

Do not weaken existing tests.

### F12 — MINOR: first-run keyring creation can lose the key (accept)
Two processes can both see `NoEntry`, generate different keys, and the loser keeps a key that is not
in the vault; its store becomes unreadable later.

**Fix.** Serialise first-run creation with an OS file lock in the app's local data directory
(`postino/keyring.lock`), held across read → generate → write → **read-back verification**. If the
read-back differs, retry once inside the lock, then fail closed. Use a small safe crate for the lock
(`fs4` or equivalent); no FFI. Unit-test the read-back mismatch path with a mock store; keep the real
vault untouched. Document the guarantee and the lock path.

## Migration and compatibility

- Schema version 3: `draft_seq` only (plus whatever F5 needs in `meta`). Additive from v2; never
  edit v1/v2 strings. Test opening a v1 and a v2 file.
- `DraftSaveArgs` changes from required to optional content fields: an early-project breaking change,
  documented in `docs/mcp.md`, README and the operation description. MCP clients re-read the schema.
- No data migration is needed for existing stores; the first open repairs the label sequence and
  orphan drafts as described.

## Out of scope

IMAP, MIME, blobs, UI, new user-facing operations beyond the `account` argument and the patch
semantics, daemon, outbox.

## Gates

- `cargo xtask ci` green, clippy `-D warnings`, no `unwrap`/`expect`/`panic!` outside tests.
- `docs/architecture.md`: transactions, revision counter, label sequence, orphan repair, keyring
  lock, what is guaranteed and what is not.
- `docs/mcp.md` and README updated for the new `draft_save` semantics and the honest send wording.
- `docs/briefs/m1-store-schema.md`/`m1-engine-wiring.md` get a short "superseded by P3d" note where
  they now differ.
- Temp files only in `%TEMP%\opencode\p3d-postino`; do not commit.

## Report

Per finding: fixed / rejected with reason. Files, tests added, commands and output tails, the schema
v3 migration, and any residual risk.
