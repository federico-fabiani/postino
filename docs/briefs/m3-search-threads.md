# Brief — P9 M3 organize: search, threads, rules

> **Package:** **P9a** (store read path, FTS5 search, search operations and UI, schema v7), then
> **P9b** (threads, conversation view, saved searches; no migration), then **P9c** (local rules on
> the same v7 tables; no second migration). **Depends on:** P7/P8 (v5, outbox), P6d (v6,
> remote-content count), P6c, P5b, P4a. **Next:** P10.
> **Owner:** developer agent · **Review:** blind engineering + UX + 100k evidence.
> No cloud, server filters or new deps: SQLCipher has FTS5.
>
> The split is deliberate: the read-path refactor plus FTS plus a query language is already a full
> package, and threads/saved searches and rules each need their own review and perf evidence. P9a
> must not grow threads or rules to save a package boundary.

## Outcome

- **P9a:** search is SQL on the encrypted store: documented language, bm25 + stable tie-break, hits,
  plain snippets, repairable index; `Engine::open` loads summaries, not bodies; 100k first page
  <100 ms.
- **P9b:** threads from references with phantom/merge handling and stable ids; saved searches with
  computed counts and recent queries.
- **P9c:** local rules with preview, idempotent runs and undo; forwarding only through P7.

## 1. Read path (P9a)

- A store-backed `Engine` holds accounts, folders, labels and `MessageSummary` rows, no bodies at
  open; `Engine::message(id)` is an owned read with a small cache. Reads are SQL for a store and
  in-memory for demo; the app caches by `(signature, revision)`, bumped on writes/reload.
- `label_summaries` reads store counts; `Message.size` serves size filters; ADR-0004.
- This refactor is the first half of P9a and gets its own benchmark before FTS lands: opening the
  current store must not load bodies, and every existing engine test must stay green.

## 2. Schema v7 and the index (P9a)

`messages` gains `search_sender`, `search_to`, `search_cc`, `search_bcc`, `attachment_names`,
`subject_norm` (TEXT NOT NULL DEFAULT ''), `in_reply_to`, `references_json` (TEXT NULL),
`ingested_at` (INTEGER NOT NULL DEFAULT 0), `rules_applied_at` (INTEGER NULL), and
`INDEX(account_id, thread_id, date)`. v7 is additive after P6d's v6; if the queue changes again,
take the next free number and update this brief before writing the migration.

**Pre-flight capability check (before the migration is written):** the bundled SQLCipher must accept
`contentless_delete=1` (FTS5 3.43+). Prove it with a throwaway probe against the workspace
`rusqlite` build and record the SQLite version. If the bundled engine is older, use external-content
FTS5 with the same columns and triggers; do not silently drop deletion correctness or the encrypted
index.

```sql
CREATE VIRTUAL TABLE message_fts USING fts5(subject, search_sender, search_to, search_cc,
  search_bcc, body_text, attachment_names, content='', contentless_delete=1,
  tokenize='unicode61 remove_diacritics 2', prefix='2 3');
```

Tables: `threads(account_id,id,subject,last_date,message_count,unread_count,starred_count,
has_attachments,latest_message_id)` PK(account_id,id); `thread_aliases(account_id,old_id,new_id)`;
`saved_searches(id,account_id,name,query,sort,created_at,updated_at)`;
`rules(id,account_id,name,enabled,priority,stop,query,actions_json,created_at,updated_at)`; journal
in §7.

- **Contentless-delete, not external content:** external content needs old values on delete and a
  content view; this deletes by rowid and makes snippets ours, so no HTML marker can reach the UI.
- **Triggers/backfill:** insert/delete/`UPDATE OF` the indexed columns delete+reinsert, FK cascades
  included; flag/folder updates do not reindex. Derived sender/recipient text, `attachment_names` and
  `subject_norm` are computed by the repository; only 1 MiB of `body_text` is indexed. v7 writes
  `meta.search_index={state:"pending"}`; `ensure_search_index` reindexes in 500-row `UPDATE` batches,
  idempotent and WAL-safe; `search_reindex` uses `delete-all`; `search_status` reports readiness/drift.
- **Encryption/build:** shadow tables are in the SQLCipher file, no sidecar; never `VACUUM` (FTS keys
  on rowid). FTS5 is compiled into the bundled SQLCipher; the exact SQLite version and
  `contentless_delete` support are recorded by the pre-flight probe above, not assumed.

## 3. Query language

Hand-written recursive-descent parser in `postino-core`, one AST, one canonical printer.

```
query := or; or := and (OR and)*; and := unary (AND? unary)*; unary := NOT unary | primary
primary := '(' query ')' | field ':' value | phrase | term
field := from|to|cc|bcc|subject|label|folder|is|has|before|after|larger|smaller
is := unread|read|starred|unstarred; has := attachment
```

- `from:giulia`, `subject:"fattura 2026"`, `label:"Lavoro urgente"`, `folder:archive`, `is:unread`,
  `has:attachment`, `after:2026-09-01`, `larger:2m`. Ids then names; dates `YYYY-MM-DD` UTC (after
  inclusive, before exclusive); sizes `k/m/g`; bare terms search all; `*` is a prefix.
- `AND/OR/NOT` uppercase; juxtaposition is AND; `NOT` needs a positive term; quotes support `\"`;
  `word:` is a field only for documented names (`Re:`, `http://` stay text).
- Errors (unclosed, dangling, bad date/size, >64 clauses, depth >8, >16 terms, >1024 B, controls) are
  `{code,position}`; UI Italian, CLI/MCP English; no echo.
- **Never interpolate raw text into `MATCH`:** quote/escape terms and bind; `parse(print(ast)) == ast`.

## 4. Execution

- `Store::search(SearchQuery)`: `message_fts JOIN messages ON rowid` + `MATCH ?1` + SQL filters
  (account, folder, label, unread, starred, attachments, date, size); filter-only skips MATCH.
- `bm25(message_fts, 4.0,3.0,2.0,1.0,0.5,1.0,2.0)` ascending, tie-break `date DESC, id ASC`;
  `sort:"date"`; `total` exact to 10k then capped; hits ≤200, offset ≤5k. Snippets in Rust from
  subject or 64 KiB of `body_text`, plain ≤240 chars, never `body_html`.
- `InterruptHandle` cancels the old query; 50 snippets max.

## 5. Threads (P9b)

- Id is the account-scoped root reference: first `References` resolving to a cached thread, else
  first valid reference, else parent thread for `In-Reply-To`, else `In-Reply-To`, else own
  `Message-ID`, else local id; preserves P5b keys.
- One-level resolution: normalize brackets, refuse controls, cap 512 B/32 refs, dedupe, ignore self;
  cycles cannot loop; an unresolved reference becomes a phantom container for waiting replies.
  Duplicate `Message-ID` uses the smallest local id. Multiple resolved threads merge into the smallest
  id (order-independent): update, delete empty, refresh, alias; `thread_get` follows aliases.
- Subject fallback only without refs, a reply prefix (`Re:`,`Fwd:`), `subject_norm` ≥8 B and one
  thread within 14 days; never merge by subject; drop it on false merges.
- `threads` is a cache refreshed with changes and reindex; counts are mailbox-wide; views include
  threads with a message in scope. Row: participants, subject, snippet, date, state, count, labels.
- `thread_get` returns ascending summaries (≤200; bodies via `message_get`); `thread_flag` read/star
  all messages (≤500); archive/delete uses ids + `message_move` in one undo. Opening marks the
  expanded message; a command marks the thread; ids change only on a merge.

## 6. Saved searches (P9b)

- Unique `(account_id,name COLLATE NOCASE)`, ≤12; label-like names. `saved_searches_list` returns
  `{id,name,query,sort,valid,error,total,unread}`; a stale query/label is `valid:false`, not a dead
  entry. Counts are computed, cached until revision changes. Applying replaces the query, clears the
  label filter and keeps the folder only if the query has `folder:`; `Esc` restores.
- Recent: last 8 successful queries per account in `meta.recent_searches:<account>` (SQLCipher);
  shown on empty focus; clear command.

## 7. Rules (P9c)

- Condition is a search query. `priority ASC,id ASC`, `stop`, ≤100/account, 1–5 actions (move, label,
  read, trash, forward 1–10 addresses); targets validate at save; deleted targets fail at run.
- Journal `rule_runs`/`rule_actions` holds ids/folders/labels/booleans only; keep 20 non-dry runs.
  Run after `sync_now` or on demand; received mail only, never drafts/Sent/outbox; no daemon, no
  Sieve. `ingested_at` at insert; `rules_applied_at` NULL for new; apply only `ingested_at >=
  rule.created_at`; manual `all` is capped.
- Per rule in order: query candidates, skip stopped, apply immediately (one transaction/message) so
  later rules see changes. Action failure records and aborts that rule's remaining actions; rules
  never fail sync. Repeats are no-ops; forward dedupes on `rule:<rule_id>:<message_id>`.
- `rules_preview` writes nothing (first 50 + effects; one rule). Undo reverses move/label/read/trash
  only when unchanged; forward never undone (report outbox id).
- Forward "Inoltra (via SMTP)", needs P7 (disabled with reason), enqueues through P7; M3 never opens
  SMTP. MCP refuses forward create/update/run unless `--allow-send`, before parsing. Adds
  `Auto-Submitted`/`X-Postino-Rule`, skips incoming with either, 50/run.

## 8. Operations, CLI, MCP

`deny_unknown_fields`; schemas from types. `search` keeps its name; `messages` remains a deprecated
alias of `hits[].message` for one milestone.

- **Search:** `search {query, account?, folder?, label?, limit?, offset?, sort?}` →
  `{query,total,total_capped,hits,messages,took_ms}`, hit `{message,snippet,highlights,matched}`;
  `search_status` → `{ready,indexed,total,last_error,size_bytes}`; `search_reindex` →
  `{documents,threads,took_ms}`.
- **Threads:** `threads_list {account?, folder?, label?, unread_only?, starred_only?,
  with_attachments?, limit?, offset?}` → `{threads,total,unread_threads,unread_messages}`;
  `thread_get {id,account?,limit?,offset?}` → `{thread,messages,total,truncated,resolved_id}`;
  `thread_flag {id,account?,unread?,starred?}` → `{thread,changed}`.
- **Saved/rules:** saved list/create/update/delete; rules list/create/update/delete;
  `rules_preview {account,query,actions,limit?}` → `{total,rows,truncated}`; `rules_run` → `{run,errors}`
  (`new|all`, ≤2000); `rules_undo`; `rules_runs`.
- Reads: `search`, `search_status`, `threads_list`, `thread_get`, saved list, `rules_list`,
  `rules_preview`, `rules_runs`; writes: `search_reindex`, `thread_flag`, saved/rule CRUD, `rules_run`,
  `rules_undo`; `rules_run` is destructive/open-world. No new op is `sensitive`; only `--allow-send`
  gates forward.
- CLI mirrors every op; `--account` when several; no secret; exit 0/1/2.

## 9. UI

- Search: `/` focus, 250 ms debounce, Enter immediate, `Esc` clears/blurs; ≤2 rows of AST chips;
  `?` help (English syntax, Italian); empty focus shows 8 recent.
- States: loading 3 skeleton rows; parse error inline with position and help; empty state; index
  building "Indice di ricerca in preparazione (x/y)".
- Saved: RICERCHE under ETICHETTE (≤12); "Salva ricerca" in field/palette; rename/edit/delete, undo.
- Threads/reading: rows with participants, date, state and count; `J/K`, Enter latest unread,
  `X/Space`, `E` archive, `U` read, `S` star; cards (one open, `J/K`, Enter, `R/A/F`, `Esc`),
  980×600 overlay; `threads_toggle`.
- Rules: full page; list + editor, 20-match preview, Save/Cancel/Delete; forward disabled without P7,
  run confirms and offers Undo.
- One command table adds search/thread/saved/rule ids. Screenshots: search, threads, thread-reading,
  saved, rules, rule-editor; dark/980.

## 10. Performance

- Fixture: 100k messages, 2 KiB bodies, 20% attachments; 20k CI variant, 100k slow.
- Windows GNU debug, 5x P50/P95: search <100 ms; `threads_list` <100 ms; `thread_get` 50 <50 ms;
  open 100k without bodies <150 ms; RSS <120 MiB.
- New 2 KiB message <20 ms; 100-message sync <2 s over baseline; backfill reported.
- Caps: query 1024 B/64 clauses/depth 8/16 terms; 200 hits/threads 100, thread messages 200, runs 50,
  offset 5k; 50 snippets/64 KiB source; thread mutations 500; saved 12; rules 100/5 actions/50
  forwards/10 recipients.

## Tests

1. Store/migration: v6→v7 fixture (schema, pending meta, version); failed v7 rolls back; reopen
   no-op; FTS5 compile + `contentless_delete` capability probe (or the external-content fallback);
   shadow tables in SQLCipher; emptied index → drift → reindex.
2. Query/ranking: fields/operators, phrases, prefix, Unicode, ids/names, dates, sizes, escaping,
   empty/long/deep/dangling/unclosed, negative-only, bad date/size, unknown ids, NUL, 10k token, FTS
   metacharacters; bm25/date+id order; snippets ≤240 chars, plain, no HTML.
3. Threads: root/reply/missing parent/cycle/self/duplicate Message-ID, safe/unsafe subject fallback,
   late parent merge, re-sync idempotent, reopen ids, UIDVALIDITY reset, alias, empty removal, counts,
   cross-folder move, flag cap.
4. Rules: priority/stop, later rule sees earlier effects, run twice idempotent, preview writes
   nothing, undo only unchanged, forward/no-SMTP/loop/caps, failure isolation, deleted target, rule
   cap, new-vs-old, manual cap.
5. Ops/CLI/MCP/UI: schemas, messages alias, alias resolution, idempotent `thread_flag`, CLI/`--json`,
   MCP list/call, forward refusal before parsing, annotations; saved CRUD/counts/undo; screens at
   980×600 light/dark, keyboard-only; no query/subject/address/body in logs; budgets; interrupt; no
   body at boot.

## Docs

README; architecture P9 (read path, FTS, grammar, threads, v7, rules), no `postino-rules`; ADR-0004;
`mcp.md` (tools, annotations, allow-send, syntax); threat-model
(index, snippets, recent, forward, loop guard, journal); ui/plan (screens, 980); ROADMAP (ticks +
numbers).

## Out of scope

Server `SEARCH`/`SORT`/`THREAD`, Sieve/ManageSieve, server rules, Gmail label/thread ids,
cross-account/attachment-content/semantic/vector/cloud/LLM search, telemetry. Daemon, scheduler,
rules while closed, notifications, auto-reply, snooze, scheduled rules. Expunge, spam classification,
server flag push, header re-fetch for pre-P9 mail, forward recall, HTML/composer, attachment preview/
save (P6c), labels (U0.5), mobile below 980×600.

## Constraints

No new crate/dependency; hand-written parser; FTS5 in SQLCipher. Store has no core dep; core
translates the AST; sync passes ids/refs. No unwrap/expect/panic/todo outside tests, no unsafe, no
input-derived slicing. Privacy: no query/subject/address/body in logs; recent only in encrypted
`meta`; snippets plain. UI: palette, Inter + JetBrains Mono, no blue/violet, Italian, keyboard-first,
980×600, light/dark. Migration: additive after v5, one transaction; index resumable. P7: one outbox
enqueue with a deterministic key; no SMTP. Temp only `%TEMP%\opencode\p9-postino`.

## Open questions (recommended answers)

1. **Cross-account search now?** No: account-scoped; `account` required with several accounts.
2. **Thread rows by default?** Yes in folder views, flat in search; with a toggle.
3. **Inert rule tables in v7 if P9c slips?** Yes; avoids a second migration.
4. **Rule forward over MCP?** Gate behind P7 `--allow-send`, refuse before parsing.

## Gates

fmt, clippy `-D warnings`, `cargo xtask ci` green; no new `Cargo.lock` package; no `VACUUM`/plaintext
index. v6→v7 + FTS5 capability and query tests; 100k evidence (P50/P95, open/RSS, index bytes,
incremental/backfill) with host/profile; a missed target is reported. Ops/CLI/MCP parity, schemas,
annotations, allow-send refusal before parsing; headless UI 980 light/dark keyboard-only, no dead ends,
screenshots; blind UX ok. Privacy log clean. P9c: one outbox row after two runs, undo evidence, no
SMTP code.

## Report

FTS design/measurements; grammar/errors; thread fixtures/merge/alias and pre-P9 limit; rule ordering/
idempotency/preview/undo/forward evidence; test map; 100k numbers.
