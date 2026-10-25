# Brief — P5c sync data-integrity closure

> **Source:** blind review of `2a0c2a7` · **Before:** P6 · **Rule:** a test that fails without each fix

## 1. MAJOR — a moved message loses its labels and can vanish from the cache

The deletion pass runs per folder and treats a missing UID as deleted. A message moved INBOX→Archive
is deleted during the INBOX pass (labels and attachments cascade) and re-inserted as new in Archive;
if the run stops before Archive it is absent from the cache altogether.

**Fix.** Delete only after every folder has been scanned, and only when the message was not seen
anywhere in this run. Track the identity (`msg_…` id / Message-ID) of every message touched, plus the
complete UID set of each folder. At the end, for each folder, `missing = cached_uids - live_uids`
and drop a missing message only if its id was not seen in any folder this run. If the run stops
early, delete nothing for the folders not fully scanned. A message without a Message-ID that moved
still gets a new id (documented; labels cannot follow), but it must not be deleted before the new
copy exists. Tests: INBOX→Archive keeps the label and read state; a run that stops after INBOX leaves
the message in the cache.

## 2. MAJOR — two distinct messages with the same Message-ID collapse

The candidate id from a Message-ID is used as the primary key, so a duplicate in the same folder
overwrites the first body and inherits its labels. `normalise_message_id` also maps different raw
values to the same key.

**Fix.**
- Hash the exact trimmed Message-ID (angle brackets removed, whitespace trimmed, no
  case-folding, no character substitution). Two raw values that differ must produce different keys.
- If the candidate id already exists **in the same folder with a different UID**, use a deterministic
  disambiguated id (`msg_<hash>_<uid>`); if that is taken, add the UIDVALIDITY. Ascending UID
  processing makes the assignment stable across syncs.
- The same id in a **different** folder is a move: update the folder, preserve labels.
- Test: two UIDs with the same Message-ID in one folder produce two rows, each with its own
  subject/body; a third sync changes nothing; labels stay on the right row.

## 3. MAJOR — UIDNEXT is trusted as truth

A missing or stale UIDNEXT hides mail above a gap (UID 250 never fetched) or deletes live mail
(`removed=1` for a live UID). A spurious `EXISTS 0` has the same effect.

**Fix.**
- Discover the true upper bound: after the hinted range, probe `UID FETCH <hint>:* (UID)` (one round
  trip) and take the maximum UID; with no UIDNEXT, start from the cursor and probe the same way.
  Batch explicit windows below the true maximum (e.g. 1 000 UIDs) so no response exceeds the untagged
  cap; check cancellation between windows.
- Never treat `EXISTS`/`RECENT` counts as liveness. Liveness comes only from a complete UID scan.
- Delete only for a folder whose entire UID scan completed; if any batch failed, skip its deletion
  pass and warn.
- Fetch flags+UIDs for the whole folder in batched `UID FETCH a:b (UID FLAGS)` windows (one query),
  compare drift in memory. This also removes the envelope re-fetch of every message on every run and
  the per-message drift query; add a repository call that returns the folder's flags in one query.
- Add `postino-net` support for a UID/flags-only fetch and for a body that no longer exists (a typed
  `NotFound`/optional body, not `Protocol`).
- Tests: missing UIDNEXT with a high UID; low UIDNEXT with a higher live UID (not deleted); empty
  mailbox; a server returning `EXISTS 0` while a UID exists; max UID discovered above the hint.

## 4. MAJOR — one expunged body aborts the whole account

A UID expunged between the envelope and body fetch returns no body; `fetch_body` maps that to
`Protocol` and the `?` aborts the run, labelled as a connection error, skipping every later folder.

**Fix.** A vanished body is a per-message skip with a warning, not a run failure. Add the typed net
error/optional result and handle it in sync. Test: two messages, the second's body vanished, the run
finishes, the first is cached, the warning names the skipped UID, later folders still sync.

## 5. MINOR — cancellation does not reach the long passes

The full flag/UID scan takes no `AtomicBool`; a cancel during a 50k folder waits for every window and
may report `Ok`.

**Fix.** Check the token between windows and before each message; a cancelled run returns
`Cancelled`, keeps committed messages and writes an error status. Test cancellation during the
UID/flags scan.

## 6. MINOR — MIME warnings are discarded

`ParsedMessage.warnings` (truncated body/HTML, too many parts, sanitizer stripped, remote content) is
never read; a message that lost parts still looks complete.

**Fix.** Map MIME warnings into the sync warnings (deduplicated per run, with the message id) and
never set `has_attachments` from a truncated part list without a warning. Test a 300-part message and
a truncated body.

## 7. MINOR — the tests miss exactly these paths

Add the cases above: moved message, duplicate Message-ID, missing/low UIDNEXT, expunged body, cancel
during the scan, MIME warnings, `max_messages` exhaustion and the deletion-pass skip.

## 8. NIT — docs and the slow test

- `crates/sync/README.md` says 16 tests, it has 17: correct it.
- `ten_thousand_messages` returns `Ok` with a "skipped" print when `POSTINO_SLOW_TESTS` is unset, so
  CI reports it green without running it. Mark it `#[ignore = "…"]` and run it with
  `cargo test -- --ignored`, or make the skip explicit in the test name/output. Document the command.

## Gates

`cargo xtask ci` green, clippy `-D warnings`, no `unwrap`/`expect`/`panic!` outside tests. Update
`docs/architecture.md` §4.6 (deferred deletion, identity disambiguation, UID discovery, cancellation,
MIME warnings) and the sync README. Temp files only in `%TEMP%\opencode\p5c-postino`; do not commit.

## Report

Per finding: fixed or rejected with reason, the failing-before evidence, the new UID scan cost on the
2k test, and the final 10k number.
