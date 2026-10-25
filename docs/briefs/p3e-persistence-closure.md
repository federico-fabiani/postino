# Brief — P3e persistence review closure

> **Source:** blind re-review of `4198360` · **After:** P2a (composer/plan files) · **Next:** P5
> **Rule:** every fix gets a test that fails without it.

## 1. MAJOR — the rollback test never rolls anything back

`save_draft_message` validates attachment ids **before** the first write, so both injected failures
(`AttachmentOwnedByOther`, `DuplicateAttachment`) abort before `upsert_message_row`. The transaction
could be split back into three calls and the suite would stay green.

**Fix.** Add a unit test inside `crates/store/src/repo.rs` (where the connection is reachable) that:
- installs a temporary SQLite trigger that raises `ABORT` on `drafts` insert,
- calls `save_draft_message` for a new message with one attachment,
- asserts the call fails and that **no** message row, attachment row, draft row or `draft_seq` row
  was written, and the folder counts are unchanged,
- drops the trigger and saves successfully.

Optionally add a second trigger on `message_labels`/`folders` if it proves a later stage. No
production fault-injection seam.

## 2. MAJOR — the InvalidArgs/Failed split is bypassed by in-memory checks

Unknown folder/account/message/label return `OpError::Failed` when the engine notices them before
the store, while the same mistake is `InvalidArgs` when the store notices it. The new tests assert
only `expect_err`.

**Fix.** Sweep every pre-check that represents a caller mistake and return `OpError::InvalidArgs`
with the specific sentence:
- `messages_list`/`search`/`message_move`: unknown folder;
- `folders_list`: unknown account;
- `message_get`/`message_flag`/`message_move`: unknown message;
- label create/update/delete/apply: unknown label or message ids (keep validation errors);
- `search`/`messages_list`: unknown label.
Keep `Failed` for configuration failures (no account, no Drafts/Sent folder, missing store,
corruption, I/O). Update the tests to assert the variant with `matches!` for each path, and update
`docs/architecture.md` if its sentence claims more than the code does.

## 3. MINOR — the keyring fast path is outside the lock

A second process can read K1 after A's write, before A's read-back; A then writes K2 and the two
disagree. The comment claims this cannot happen.

**Fix.** Take the same file lock for the **first read** as well, so the whole load-or-create is
serialised; keep the read-back. On a read-back mismatch (only possible if something rewrote the
vault without the lock), re-read and adopt the stored key if it is a valid 32-byte key; only if the
stored value is still not ours after one retry, fail closed. Remove the misleading comment and
document the guarantee precisely.

**Tests.** Prove exclusivity of `lock_first_run` with two threads using `try_lock_exclusive` (second
must fail while the first holds it, succeed after release); prove the keyring suite never writes
outside its temp directory by porting the integration cases to unit tests that pass a temp lock
path; prove the adopt-on-mismatch path with a mock store that returns a different valid key once.

## 4. MINOR — orphan-repair branches and label write-through are untested

Add:
- open a store whose orphan draft names an unknown account → readable `Failed`, engine not started;
- open a store whose orphan draft has no Drafts folder → readable `Failed`;
- after a successful repair, apply a label and set a flag through the engine, reopen, assert both
  persisted and that store folder counts agree with the engine.

## 5. MINOR — patch updates still clear fields they do not own

`body_html` is hard-coded to `None` and `in_reply_to` to `None` on every draft update, so a
subject-only patch destroys them.

**Fix.** Preserve `body_html` from the current message in `ops`/engine; preserve `in_reply_to` in
`save_draft_row` when the incoming value is `None` (`COALESCE(excluded.in_reply_to,
drafts.in_reply_to)`) and document that clearing it needs an explicit field, which does not exist
yet. Test a stored draft with both set, patched with only a subject, and assert they survive a
reopen.

## 6. MINOR — docs describe schema v2 and the wrong draft API

`crates/store/README.md` says v2 and documents `save_draft (revision++)` as the way to save a draft;
following it writes only the revision row and creates the ghost F7 repairs. `docs/store-spike.md`
also says v2.

**Fix.** Update the README to schema v3, list `save_draft_message`/`send_draft_message`/`delete_draft`
and `draft_seq`/`label_seq`/orphan repair, and mark the half-write pattern as removed. Update the
spike outcome and any architecture sentence that still says v2.

## 7. MINOR — send-button honesty is incomplete before the click

The composer now says “Invia (locale)” but the plan still says “demo in M0; SMTP in M2” and nothing
near the button explains that no message leaves the machine.

**Fix.** Keep the label, add a one-line caption or tooltip next to it: “Nessun invio SMTP: il
messaggio resta in Inviati su questo computer.” Update `docs/ui/plan.md` to the same wording and
remove the stale “demo” phrasing.

## Gates

`cargo xtask ci` green, clippy `-D warnings`, no `unwrap`/`expect`/`panic!` outside tests. Update
`docs/architecture.md` only where behaviour changed. Temp files only in
`%TEMP%\opencode\p3e-postino`; do not commit.

## Report

Per finding: fixed or rejected with reason; tests added; evidence for the rollback and keyring
races; docs updated.
