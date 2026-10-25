# Brief — M1 engine wiring (P3c)

> **Depends on:** P3b (store schema + repository + keyring) · **Next:** P4 MIME
> **No UI redesign; the demo path must stay byte-for-byte behaviour-compatible**

## Outcome

`postino-core` can run on the encrypted store instead of the in-memory demo vectors. The same
operations, CLI and tests keep working; a process restart preserves accounts, folders, messages,
labels, drafts and flags.

## Design

- `Engine` gains `store: Option<postino_store::Repo>`.
- `Engine::demo()` and `Engine::demo_at()` are unchanged (`store: None`, in-memory).
- New `Engine::open(store: Repo) -> Result<Self, OpError>`: load accounts, folders, messages with
  attachments, labels and drafts into the existing vectors, then keep the repo for write-through.
- **Write-through, not lazy sync:** every mutating engine method writes to the repo first and only
  then updates memory, so a failed write leaves both sides unchanged. Map `StoreError` to
  `OpError::Failed` with an actionable message.
- Wire at least: `save_draft`, `send_draft`, `set_flags`, `move_message`, `move_messages` (if
  present), `delete_message` (if present), label create/update/delete/apply, and the automatic
  read/star paths. Account/folder writes are for P6; expose `upsert_account`/`upsert_folder`
  internally or leave a documented seam.
- Counts: keep the in-memory folder counts as today; after a write-through that changes them,
  recompute locally (do not re-read the whole store per mutation).
- Loading: one query per table; attachments grouped by message; labels by message. No N+1 per
  message.
- The store crate stays independent of core; core maps between its model and store structs in one
  module (`store_map.rs` or similar), not scattered.

## CLI / app

- CLI: add `--store <path>` (or the brief's better name) to open a store instead of `--demo`; the
  key comes from `postino_platform::load_or_create_key("default")`. Without `--demo` and without a
  store, keep today's empty-engine behaviour until onboarding (P6). Never print the key or the path
  contents.
- The desktop app stays on the demo until P6; document that clearly.
- Tests must never touch the real keyring: construct the store with a random `StoreKey` in a temp
  dir.

## Tests

- Engine round trip: demo-shaped fixture written into a temp store, drop the engine, reopen, assert
  accounts, folders, counts, messages, recipients, labels, attachments and flags survived.
- Mutations: save draft → reopen → same draft id and revision; send draft → reopened in Sent; label
  apply/remove → survives; move → folder and counts survive; mark read → survives.
- Failure: a repo error surfaces as `OpError` and memory is unchanged (inject a closed/bad store or
  an unknown id).
- Demo tests unchanged and still pass.
- CLI: open a temp store, create a draft, reopen the process engine (unit-level is fine), list it.

## Gates

- `cargo xtask ci` green, clippy `-D warnings`, no `unwrap` outside tests.
- `docs/architecture.md` and `docs/development.md` updated: what is persisted, what is still demo,
  the keyring flow, and the P3c limits (single process, no daemon).
- README honesty: "store exists and the engine can run on it; onboarding is the next step".

## Out of scope

IMAP, MIME parsing, onboarding UI, daemon, FTS5, attachments bytes, SMTP, scheduled send.
