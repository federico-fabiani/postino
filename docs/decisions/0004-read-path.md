# ADR-0004 — The list is a summary; the body is an owned read

**Status:** accepted (P9a) · **Date:** 2026-10-26

## Context

Until P8 the store-backed engine loaded every message body, HTML part and attachment row into one
`Vec<Message>`, because the only search was a linear scan over that vector. The design made opening a
mailbox proportional to the size of every cached body and made FTS impossible without a second source
of truth. P9a must search 100 000 messages in the encrypted store while keeping the mail list fast.

## Decision

1. `Engine::open` loads `MessageSummary` rows, labels, attachment id/hash metadata and draft metadata
   (id, revision, updated_at, in_reply_to). It does not read `body_text`, `body_html`, attachment rows
   beyond that id/hash query, or blob bytes. Orphan-draft repair reads a body only for a row that has
   no display message.
2. `Engine::message(id)` returns an owned `Message`. A store-backed engine reads
   `Store::get_message_with_attachments` and keeps an eight-entry cache keyed by an engine revision.
   Every write and reload bumps the revision, so a cached body cannot outlive a mutation.
3. The demo engine keeps its full synthetic messages in memory and answers through the same API. The
   CLI, MCP and desktop app never branch on the source.
4. A dedicated draft-revision map, not the body cache, is the stale-draft guard. It is loaded from
   metadata at open and updated on every save; an edited draft is refused even after the cache has
   evicted or a write has cleared it. Bodies are read only when a draft is opened or serialized.
5. Search is SQL over FTS5 in the same SQLCipher file. The store owns schema, triggers, ranking and
   pagination; core owns the query language and translates its AST into a typed filter tree. No caller
   text is interpolated into SQL or `MATCH`.
6. The index is contentless-delete and repairable (`meta.search_index`, batched 500-row backfill,
   `search_reindex`). The date/id tie-break lives in a small auxiliary rank table maintained by the
   same triggers, so the SQL order is exact before the page. Snippets are computed in Rust from the
   plain body, never `body_html`. Opening a pending index does not block the mailbox; batches run from
   the shell and from search.

## Consequences

- The list and search first page are independent of body size; the 100k relevance page meets the
  100 ms target on the development host.
- A body read is one SQL round trip on first access and a cache hit afterwards. The app's reading pane
  must tolerate an owned message instead of borrowing from the gallery.
- `Engine` is not `Sync` (the cache is a `RefCell`); it remains `Send`, so the CLI and workers can move
  one engine to their own thread.
- A covering summary index keeps the 2 KiB body pages out of the open scan. The measured 100k open is
  268–357 ms on the development host, above the 150 ms target: the remaining cost is decrypting and
  decoding 100k summary rows and building the engine's Vec/index/counts, not reading bodies. A compact
  summary table or body table is the next step if the target must be met; the miss is reported, not
  hidden.
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
