# ADR-0004 — The list is a summary; the body is an owned read

**Status:** accepted (P9a) · **Date:** 2026-10-26

## Context

Until P8 the store-backed engine loaded every message body, HTML part and attachment row into one
`Vec<Message>`, because the only search was a linear scan over that vector. The design made opening a
mailbox proportional to the size of every cached body and made FTS impossible without a second source
of truth. P9a must search 100 000 messages in the encrypted store while keeping the mail list fast.

## Decision

1. `Engine::open` loads `MessageSummary` rows (one query, plus labels, blob hashes and drafts). It does
   not read `body_text`, `body_html`, attachment rows or blob bytes.
2. `Engine::message(id)` returns an owned `Message`. A store-backed engine reads
   `Store::get_message_with_attachments` and keeps an eight-entry cache keyed by an engine revision.
   Every write and reload bumps the revision, so a cached body cannot outlive a mutation.
3. The demo engine keeps its full synthetic messages in memory and answers through the same API. The
   CLI, MCP and desktop app never branch on the source.
4. Drafts are preloaded at open: the outbox compares the bytes this engine loaded, so re-reading at
   send time would hide another process's edit. A draft is small and bounded; a mailbox is not.
5. Search is SQL over FTS5 in the same SQLCipher file. The store owns schema, triggers, ranking and
   pagination; core owns the query language and translates its AST into a typed filter tree. No caller
   text is interpolated into SQL or `MATCH`.
6. The index is contentless-delete and repairable (`meta.search_index`, batched 500-row backfill,
   `search_reindex`). Snippets are computed in Rust from the plain body, never `body_html`.

## Consequences

- The list and search first page are independent of body size; the 100k relevance page meets the
  100 ms target on the development host.
- A body read is one SQL round trip on first access and a cache hit afterwards. The app's reading pane
  must tolerate an owned message instead of borrowing from the gallery.
- `Engine` is not `Sync` (the cache is a `RefCell`); it remains `Send`, so the CLI and workers can move
  one engine to their own thread.
- SQLite stores a 2 KiB body inline in the `messages` table, so a summary scan still touches those
  pages. The measured 100k open is above the 150 ms target; moving bodies to a separate table (or a
  compact summary table) is the documented next step, not silently claimed.
- FTS5 shadow tables are encrypted with the database. There is no sidecar and `VACUUM` is forbidden
  because FTS keys on `messages.rowid`.

## Rejected

- **Keep the in-memory body vector and filter in Rust.** Cannot meet the 100k gate and duplicates the
  encrypted source of truth.
- **External-content FTS.** Needs old values and a content view on delete; delete+insert on a
  contentless-delete table is simpler and keeps snippets out of the index.
- **Move bodies to blobs now.** Adds an open/decrypt round trip to every read and a second failure mode;
  a future body table is the smaller change.
- **A second search database.** Violates the encrypted single-store invariant and adds a sidecar.
