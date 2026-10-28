# MCP

Postino exposes the same operation registry as the app and the CLI through an opt-in MCP server.

## Start it

```sh
postino mcp
postino --demo mcp
postino --store <path> mcp
postino --store <path> mcp --allow-send
postino --store <path> mcp --compact --audit-log <path>
```

- Transport: newline-delimited JSON-RPC 2.0 on stdin/stdout.
- It opens no port, starts nothing else, and stops at EOF.
- `--compact` removes titles, annotation hints and schema prose from `tools/list`; names, types,
  required fields and enums stay, so the list is smaller without becoming unusable.
- Every `tools/call` and `resources/read` is appended to `mcp-audit.jsonl` beside the store by
  default (`--audit-log` overrides it; demo uses the OS temp directory). The log stores the
  timestamp, event, tool/resource name and outcome, never arguments, bodies or credentials. It
  rotates to `.jsonl.1` at 8 MiB. A failed audit write is reported on stderr and does not change a
  successful operation into a failure.
- A session allows 300 tool/resource calls per rolling 60-second window. Exceeding it returns an
  `isError` result (or a JSON-RPC error for a resource) without calling the operation.
- Logs, if any, go to stderr. stdout carries protocol messages only.
- When a daemon is running, this process is a daemon client: it reads `<store>.token`, speaks the
  same JSON-RPC 2.0 over the per-user pipe/socket and calls `op`. The tools, schemas and error shape
  are the daemon's single registry.
- When the daemon is absent, reads use the local encrypted cache directly and **every mutation returns
  a readable `isError` tool result** ("il daemon non è in esecuzione…"). MCP never starts a daemon
  implicitly and never opens a TCP port. The local reader is opened read-only.
- Without `--demo` and `--store`, `postino mcp` targets the default encrypted store at
  `%LOCALAPPDATA%\Postino\mail.db` on Windows. Windows Credential Manager is the only keyring backend
  in this build; macOS and Linux fail at the platform seam until their native keychains land with
  packaging, so there is no platform-equivalent default store yet. `--store` picks another database.
  `--demo` stays synthetic and never touches the store.
- The process needs the store key from the OS keyring only when it reads the store locally. A daemon
  client needs the token file, not the key. Neither is ever printed.

Client configuration:

```json
{
  "mcpServers": {
    "postino": {
      "command": "postino",
      "args": ["--demo", "mcp"]
    }
  }
}
```

## Protocol

Implemented:

- `initialize` with protocol versions `2025-06-18`, `2025-03-26`, `2024-11-05`.
- `ping`.
- `tools/list`, with `inputSchema` derived from Rust request types and all four annotation hints.
- `tools/call`, returning a text block plus `structuredContent`; tool failures are
  `{ "isError": true }` results, not JSON-RPC errors.
- `notifications/initialized` and `notifications/cancelled` are accepted and ignored.

Unknown methods return JSON-RPC `-32601`; malformed arguments return `-32602`; a parse error
returns `-32700`. A tool that fails at runtime returns a normal result with `isError: true`, so the
agent can read the message and recover.

## Tools

| Tool | Read-only | Purpose |
|---|---|---|
| `app_info` | yes | Version, platform, data directory, demo and store flags, operation count |
| `accounts_list` | yes | Accounts with provider, address, IMAP host/port/TLS and last sync status; no secret |
| `account_test` | yes | Log in with the stored credential and list folders; returns the count and capabilities |
| `sync_now` | no | Run one sync for an account; keeps the previous cache on failure |
| `sync_status` | yes | Persisted sync phase, last attempt/success, error and folder progress |
| `folders_list` | yes | Folders with unread and total counts |
| `messages_list` | yes | Message summaries, paginated; `label` alone spans every folder, `folder` narrows it |
| `message_get` | yes | One message with body and attachments |
| `message_export` | yes | One message as an RFC 5322 `.eml` in bounded base64 (10 MiB); byte-exact when the v9 raw blob was retained, otherwise reconstructed from sanitized text/HTML, threading headers and attachments |
| `message_import` | no | Store one `.eml` (bounded base64, 10 MiB) in the account's local **Import** folder through the ordinary ingest path; a duplicate Message-ID never moves existing mail and the copy is marked read |
| `message_unsubscribe` | yes | RFC 2369/8058 `List-Unsubscribe` targets captured at ingest (`mailto`, `https`, `http`) and the One-Click flag; sends and opens nothing |
| `attachment_get` | yes | One attachment's metadata and exact bytes as bounded base64 (25 MiB cap); `postino attachment get --out` writes the same bytes |
| `message_pgp_verify` | yes | Verify a received PGP/MIME signature against the stored public keys using the exact v9 bytes; `kind` says `signed`/`encrypted`/`none`, state is `verified`/`bad`/`no_key`/`unknown`/`not_stored` |
| `pgp_keys_list` | yes | Stored key metadata: id, fingerprint, key id, primary user id, secret presence and protection; never key material |
| `pgp_key_export_public` | yes | One stored public certificate as ASCII armor |
| `pgp_key_delete` | no | Delete one stored key, public or private; no key discovery or fetch |
| `message_invite` | yes | Parse the first `text/calendar` attachment (≤ 256 KiB, 1 000 components, 2 000 attendees, 64-character organizer/attendee lines); `invite` is `null` for malformed or hostile input and the file stays an attachment |
| `invite_reply_draft` | no | Draft accept/tentative/decline to the organizer only, with `text/calendar` METHOD:REPLY; only a received REQUEST is answerable, a Sent copy is informational; never sends and never updates a calendar |
| `message_flag` | no | Set read/unread and/or starred |
| `message_move` | no | Move a message to a regular folder |
| `draft_save` | no | Create a draft or patch one in place; metadata only, including the optional `in_reply_to` |
| `draft_attachment_add` | no | Store one file's exact bytes (bounded base64) in the encrypted blob store; 20 files, 25 MiB each, 25 MiB total |
| `draft_attachment_remove` | no | Remove one attachment row by id or exact filename; the blob waits for a later GC |
| `account_signature_set` | no | Set an account's plain-text signature (≤ 4 KiB, newline/tab only) |
| `message_send` | no | Queue a draft for SMTP submission with a 5 s undo window; disabled unless `--allow-send` |
| `send_schedule` | no | Queue a draft at an RFC 3339 `send_at`; disabled unless `--allow-send` |
| `outbox_list` | yes | Journal rows newest-first with state, attempts, a fixed error, recipient count and subject; `limit`/`offset` page the history; offline |
| `outbox_cancel` | no | Discard a still-queued or permanently failed row; the draft stays in Drafts |
| `outbox_retry` | no | Retry a failed/uncertain row with `confirm: true`; disabled unless `--allow-send` |
| `outbox_mark_sent` | no | Resolve an uncertain row locally without contacting the server; disabled unless `--allow-send` |
| `send_dry_run` | yes | Exact RFC 822 bytes (base64) and envelope without queueing |
| `search` | yes | FTS5 search with fields, phrases, prefixes and AND/OR/NOT; bm25 order, plain snippets, pageable |
| `search_status` | yes | Index readiness, FTS/rank row-id drift, document/message counts, database+WAL size, last error and last index update |
| `search_reindex` | no | Delete and rebuild the encrypted FTS and rank indexes in resumable batches; single-process only |
| `threads_list` | yes | Conversation rows with participants, subject, snippet, date, state, mailbox-wide counts and labels; folder/label/flag scope, pageable |
| `thread_get` | yes | One conversation and an ascending page of ≤ 200 message summaries; old merged ids resolve through aliases |
| `thread_flag` | no | Set read/starred on every message of a thread (≤ 500); idempotent, returns the refreshed row and the real transition count |
| `saved_searches_list` | yes | Saved searches with computed total/unread counts, `valid`/`error` for a stale query, `index_pending` while the FTS index builds, and the eight recent queries |
| `saved_search_create` | no | Save a named query (≤ 12 per account, unique ignoring case, atomic cap; query validated now) |
| `saved_search_update` | no | Change the name, query and/or sort of one saved search; `account` is required in a multi-account mailbox |
| `saved_search_delete` | no | Delete one saved search; `account` is required in a multi-account mailbox, local metadata only |
| `saved_search_recent_clear` | no | Forget one account's recent queries from the encrypted `meta` table |
| `rules_list` | yes | Local rules in `priority ASC, id ASC` with actions and computed `valid`/`error` |
| `rules_create` | no | Create a validated query plus 1–5 actions (≤ 100 rules; forward gated by `--allow-send`) |
| `rules_update` | no | Change name/query/actions/priority/stop/enabled; a forward payload or enabling a stored forward rule is gated by `--allow-send` |
| `rules_delete` | no | Delete one local rule; past runs stay in the journal |
| `rules_preview` | yes | First 50 matches plus one typed effect per action; writes nothing |
| `rules_run` | no | Run enabled rules (`new`/`all`) and return the journaled run plus bounded errors; disabled unless `--allow-send` |
| `rules_undo` | no | Destructive and one-way: reverse a run's move/label/read/trash where the message is unchanged; forward ids are reported |
| `rules_runs` | yes | Newest ≤ 20 runs with mode, counts, errors, finish and undone flag |
| `labels_list` | yes | Labels with mailbox-wide total and unread counts |
| `label_create` | no | Create a local label with a semantic colour |
| `label_update` | no | Rename and/or recolour a label; the id is stable |
| `label_delete` | no | Delete a label, detach it from every message, return the affected count |
| `label_apply` | no | Apply or remove one label on 1–500 messages; idempotent |
| `templates_list` | yes | Local compose templates (≤ 20) sorted by name; demo returns an empty list |
| `template_create` | no | Create a local template with a validated name, subject and plain-text body in the encrypted store |
| `template_update` | no | Change the name, subject and/or body of a stored template; id and creation time stay stable |
| `template_delete` | no | Delete one stored template; drafts that already used it do not change |

## Sending over MCP

Sending is an explicit opt-in:

```sh
postino mcp --allow-send
```

Without the flag, `message_send`, `send_schedule`, `outbox_retry`, `outbox_mark_sent` and
`rules_run` are still listed (so an agent can see why they are unavailable) but carry
`"enabled": false` with a `disabledReason`, and `tools/call` refuses them before parsing the
arguments. `outbox_list`, `outbox_cancel`, `send_dry_run`, non-forward rule CRUD and `rules_preview`
stay available: a dry run writes nothing, a preview writes nothing and a cancel discards a queued or
permanently failed submission without sending. `outbox_retry` is gated because it can send stored
bytes, `outbox_mark_sent` because it resolves an uncertainty a person should resolve, and `rules_run`
because an enabled rule may forward. A `rules_create`/`rules_update` payload containing a forward
action is refused before the typed parser runs, and so is an enable-only `rules_update` of a stored
rule whose actions already include `forward`: the server reads the rule id from the raw arguments and
checks the stored action list, so a client cannot arm a dormant forward rule without the flag. The
automatic `new` pass inside `sync_now` obeys the same gate: with `--allow-send` absent, a forward is
recorded as deferred and the sync still succeeds. The daemon client carries the flag in its
handshake, so a daemon-side sync cannot bypass this client gate. Create or enable the forward rule
from the CLI or the desktop instead.

**MCP only queues.** The server has no timer and never runs the outbox pass. A `message_send` or
`send_schedule` call writes the journal row and returns; the message leaves the machine when the
desktop app (which polls the store every two seconds) or `postino send` processes it. `outbox_list`
therefore shows `queued` until one of those runs, and `outbox list` from the CLI is read-only — it does
not drain either.

`message_send` is **not** marked destructive: sending is the product's purpose, and the annotation
would mislead an agent that treats destructive as "deletes data". The guardrail is the server flag,
the recipient/message caps, the outbox row and the explicit confirmation on retry/mark-sent.

## Attachments and signatures over MCP

`draft_attachment_add` accepts `data_base64`, never a filesystem path: an MCP client chooses the bytes
it sends and the server never reads the user's disk. The server reads each JSON-RPC request line with a
hard ceiling of about 37 MB (enough for a 25 MiB payload plus base64 and JSON framing); an oversized
line is drained and answered with a readable JSON-RPC error instead of being allocated in full. The
base64 string is bounded before it is decoded, an invalid alphabet is refused, and the decoded size is
checked again against the per-file and total caps. The operation is not sensitive — the bytes are not
a credential — but its description states that the payload leaves the caller and is stored only in
the encrypted local blob store. A duplicate filename is renamed, a hostile filename is sanitized, and
an invalid declared MIME falls back to the extension (unknown → `application/octet-stream`). The
engine accepts a declared type only when it matches the extension table, so a valid lie such as
`evil.exe` + `image/png` is stored as `application/octet-stream`. The
result is the updated draft; read the attachment list from `draft_save`/`message_get` afterwards.

`draft_attachment_remove` takes the draft and an attachment id (or exact filename) and returns the
updated draft. It deletes the row, not the deduplicated blob; there is no GC yet.

`account_signature_set` sets one account's plain-text signature. HTML, images and tracking are not
supported. The change applies to new messages; an existing draft keeps the text it saved.

`draft_save` remains metadata-only for compatibility. Passing attachment metadata without stored bytes
is accepted for the row but `message_send` refuses it with the filename before writing a queue row.

## Sensitive operations

`account_add`, `account_update`, `account_remove`, `pgp_key_import` and `message_pgp_decrypt` are
marked `sensitive` in the operation registry. They are **not** in `tools/list`, and `tools/call`
refuses them before it parses the arguments, with a readable `isError` result that never repeats the
arguments. For the account operations the reason is structural: MCP cannot see or set a password, and
it cannot create, reconfigure or remove an account. For the two PGP operations it is the per-use
passphrase: MCP has no no-echo prompt and must not receive a secret in an argument. The same registry
entries remain available to the CLI (`postino account add|update|remove`, `postino pgp ...`), whose
password path is a no-echo prompt or one `--password-stdin` / `--passphrase-stdin` line; there is
deliberately no `--password` or `--passphrase` argv flag, and `postino run` refuses sensitive
operations and any send that carries a `pgp.passphrase`, so generic JSON arguments cannot carry a
secret into shell history. `account_test` is not sensitive because it never receives a password: it
uses the credential already in the OS keyring.

`message_send`, `send_schedule` and `send_dry_run` accept an optional `pgp` object to sign and/or
encrypt. A request that carries a `pgp.passphrase` is refused over MCP before dispatch and by
`postino run` after parsing, because neither path has a no-echo prompt; an agent can still ask for
encryption by key id, or prepare a draft and leave signing to the CLI or the desktop app.

Annotations:

- `readOnlyHint`: true for `accounts_list`, `account_test`, `sync_status`, `outbox_list`,
  `send_dry_run`, `rules_list`, `rules_preview`, `rules_runs`, `message_pgp_verify`,
  `pgp_keys_list`, `pgp_key_export_public` and every read.
- `destructiveHint`: true for `account_remove` (it deletes the local cache for one account),
  `pgp_key_delete` (it deletes stored key material), `rules_run` (it changes local mail and may queue
  a submission), `rules_undo` (it moves, flags and
  labels mail and is one-way, with no redo) and `rules_delete` (rule metadata only). Sending and
  message deletion are not marked destructive: `message_send` is the product's purpose and
  `outbox_cancel` discards a queued submission, not mail.
- `idempotentHint`: true for reads, `message_flag`, `label_apply`, `label_update` and `outbox_list`.
  It is false for `outbox_cancel` and `rules_run`: a second cancel finds the row already `cancelled`,
  and a run writes a new journal row even when every action is a no-op.
- `openWorldHint`: true for `account_test`, `sync_now`, `outbox_retry` and `rules_run`, which can
  reach the user's own server. `message_send` and `send_schedule` only queue, so they are local; the
  drain that opens SMTP is the app or the CLI. Everything else is local.

`labels_list`, `label_create`, `label_update`, `label_delete` and `label_apply` are backed by the
same registry the app and the CLI use. Labels are local metadata: in memory in the demo, in the
encrypted store with `--store`, and never sent to a server. Label ids are stable for the life of the
label and never reused, so an id from a deleted label is dead. Names are trimmed, at most 48
characters, unique ignoring case, and refuse control, bidi and format characters (U+061C,
U+200B–U+200F, U+202A–U+202E, U+2060–U+206F, U+FEFF) that could reorder or hide text.

`messages_list` accepts an optional `label` id. With only `label` it returns every folder; with
`folder` and `label` it returns their intersection; with neither it defaults to `inbox`. An unknown
folder or label id is an error instead of an empty list. `search` also accepts `label` and applies it
before the row limit, so a match outside the first 200 rows is still returned. `label_apply`
validates every message id before changing anything, caps a call at 500 ids and returns how many
messages actually changed, so an agent can retry safely.

### Search syntax

`search` takes `{query, account?, folder?, label?, limit?, offset?, unread_only?, with_attachments?, sort?}`
and returns `{query,total,total_capped,hits,messages,took_ms,index?}`. `index` is present only while the
FTS build is pending: hits are empty and the object carries state/counts/last_error. A hit is
`{message,snippet,highlights,matched}`; `messages` is the deprecated `hits[].message` alias for one
milestone. The grammar is:

```
from: to: cc: bcc: subject: label: folder:
is:unread is:read is:starred is:unstarred
has:attachment
before:YYYY-MM-DD after:YYYY-MM-DD
larger:2m smaller:500k
"a phrase"  prefisso*  NOT  AND  OR  (…)
```

Bare terms search every indexed text column. `AND`/`OR`/`NOT` are uppercase; juxtaposition is `AND`;
quotes support `\"`; `*` is a prefix. Dates are UTC (`after` inclusive, `before` exclusive). Label and
folder values accept an id or an exact name. The total is exact up to 10 000 and `total_capped` says
when it is not; hits are at most 200 and offset at most 5 000. `sort` is `relevance` (bm25, default) or
`date`. Snippets are plain text, never HTML. A parse error is an invalid-argument result naming the
error code and byte position; the query itself is never echoed into a log. The order is the exact
`bm25` plus `date DESC, id ASC` global tie-break (the rank table is maintained in FTS5), and `unread_only`
/ `with_attachments` are filters in the same scope as the page. `search_status` is offline and
read-only; `search_reindex` is a local write and never contacts a server. It is a single-process
maintenance action: do not run it from the desktop and MCP at the same time.

## Conversations and saved searches over MCP

`threads_list` returns one row per conversation with `{id, subject, snippet, participants, last_date,
message_count, unread_count, starred_count, has_attachments, latest_message_id, labels, state}`.
`folder`, `label`, `unread_only`, `starred_only` and `with_attachments` decide which conversations are
listed; the counts on a row stay mailbox-wide. `total`, `unread_threads` and `unread_messages` describe
the filtered scope. `thread_get` returns an ascending page of ≤ 200 message summaries; the desktop asks
for the newest page first and prepends earlier pages, and `truncated` says whether more exist. Bodies
are read with `message_get`. An id merged into another conversation still resolves, and `resolved_id`
is the current one. `thread_flag` sets `unread` and/or `starred` on every message of one thread
(≤ 500), updates the engine's folder/label/star counts, returns the refreshed row and the number of
real transitions, and a repeat returns `changed: 0`. Thread archive/delete is not a separate tool: call
`message_move` for the ids returned by `thread_get` (the desktop does this over the whole thread, with
one undo; a partial batch still offers undo for the rows that moved).

`threads_list`, `thread_get` and `saved_searches_list` are read-only. `thread_flag`,
`saved_search_create`, `saved_search_update` and `saved_search_recent_clear` are local writes, and
`saved_search_delete` is the only destructive one (metadata only). None is sensitive and none opens a
network connection. MCP marks `thread_flag` idempotent.

`saved_searches_list` returns `{searches, recent}`. A search whose label or folder was deleted later
is `valid: false` with `error` and zero counts; it is not hidden. A valid text query while the FTS index
is still building is `valid: true, index_pending: true` with zero counts, not a stale entry.
`saved_search_create` takes `{account?, name, query, sort?}` and validates the query (parse plus
label/folder resolution) before storing it; names are 1–48 characters, unique per account ignoring
case, and the 12-row cap is checked in the same store transaction as the insert. `sort` is `relevance`
or `date`. `saved_search_update` and `saved_search_delete` take `{id, account?, ...}`; `account` is
required when the mailbox has several accounts and the row must belong to it. `recent` holds the last
eight successful queries in the encrypted `meta.recent_searches:<account>` value; only an explicit
commit records one, and `saved_search_recent_clear` forgets them. Deleting an account deletes that key.

## Local rules over MCP

A rule is `{name, query, actions, priority?, stop?, enabled?}`; `query` is the same search language and
`actions` is 1–5 objects, at most one per kind:

```json
{"kind":"move","folder":"archive"}
{"kind":"label","label":"lbl_3"}
{"kind":"read"}
{"kind":"unread"}
{"kind":"trash"}
{"kind":"forward","to":[{"name":"","email":"dest@example.com"}]}
```

Names are 1–48 characters, unique per account ignoring case (including Unicode lowercase, enforced by
`rules.name_key`), and an account holds at most 100 rules. Move targets must be regular folders of the
same account (not virtual, Drafts or Sent) and accept an id or exact name; a label target must exist
and belong to the same account; forward takes 1–10 distinct addresses. A target deleted later leaves
`valid:false` with a readable `error`; `rules_run` records the failure and carries on with the later
rules. `rules_update` re-reads the store row, refuses a stale edit with “changed in another session”
and validates the merged action list.

`rules_run` takes `{account?, scope?: "new"|"all"}`. `new` is the default and considers received mail
ingested after the rule was created and not yet applied; a message is marked applied only when every
action completed, so a failed or deferred action is retried by the next `new`. `all` reads up to 2 000
candidates per rule and keeps a per-rule cursor; repeated runs advance through a larger backlog, retry
a page that ended in a deferred forward, and wrap after the tail. Only received-origin mail is
eligible: a Sent copy moved to Archive is still `sent` and stays excluded; Drafts, Sent, Trash folders
and outbox rows are excluded too. Rules run in `priority ASC, id ASC`; `stop` makes later rules skip a
message the rule matched. Each rule queries fresh candidates, so a later rule sees an earlier rule's
move, label or flag. A repeated action is a no-op, and a forward resolves to the deterministic outbox
key `rule:<rule>:<message>` plus a durable encrypted dedupe marker that survives outbox pruning. Only
new outbox rows count toward the 50-per-run budget; over-budget forwards are deferred to the next run
instead of failing or being consumed. A missing SMTP endpoint fails the action instead of queueing. The
result is `{run, errors}`; `errors` is capped at 200 sentences with an omitted-count line while
`run.errors` stays exact. `rules_runs` lists the newest 20 runs, and deletion keeps that history. A
forward's bytes carry `Auto-Submitted: auto-forwarded` and `X-Postino-Rule: <rule_id>`; incoming mail
carrying either header is skipped for forwarding and the fact is stored only in the encrypted `meta`
key `rule_guard:<account>:<message>`. Bulk sync deletion, a folder reset and account removal clean
those keys.

`rules_preview {account?, query, actions, limit?, rule?}` returns `{total, rows, truncated}` with one
effect per action (`apply`, `no_op`, `skip`) and writes nothing. With `rule`, it applies that rule's
`created_at` horizon; without it, it previews all received mail as a hypothetical new rule. A forward
is `skip` when the account has no SMTP endpoint or the message is loop-guarded, and a move updates the
simulated folder so a later `trash` after a move to Trash is `no_op`. `rules_undo {id, account?}`
reverses a run's move/label/read/trash actions only for messages still in the state the run left,
including starred state; it returns `{run, restored, skipped, forwards}` where `forwards` names outbox
rows that were deliberately not undone. `rules_run` is destructive/open-world, `rules_undo` and
`rules_delete` are destructive, `rules_preview` and `rules_runs` are read-only, and none is sensitive.
Without `--allow-send`, `rules_run`, any forward payload in `rules_create`/`rules_update`, an
enable-only update of a stored forward rule, and any forward reached by a `sync_now` rule pass are
refused or deferred.

`draft_save` refuses CR/LF in addresses or the subject, validates every recipient, and returns the
draft with a stable id. **It is a patch on update:** every content field (`to`, `cc`, `bcc`,
`subject`, `body_text`, `attachments`, `in_reply_to`) is optional; omitting a field keeps its current
value and sending it — even as an empty list or empty string — replaces it, with one exception:
`in_reply_to` cannot be cleared in P8. Omitting it keeps the stored parent; an empty string is
rejected because there is no explicit clear field yet (P9 adds the full thread model). On create,
omitted fields are empty, and `account` chooses the account; it is required when more than one account
exists. `attachments` is metadata only: the bytes come from `draft_attachment_add`, and a row without
a stored blob is refused at send time with the filename. `in_reply_to` is a parent `Message-ID`,
normalised without brackets and emitted as `In-Reply-To`/`References`. The app sends the full content
on every autosave, which is equivalent to replacing every field.

`message_send` requires at least one recipient and a configured SMTP endpoint; it serializes the
draft, stores the exact bytes, inserts an outbox row and returns the row with its 5-second undo
window. A draft with a live journal row cannot be edited or moved; a permanently failed row can be
discarded with `outbox_cancel` to make the draft editable again, and a Sent copy is ordinary mail.
`send_schedule` does the same at an RFC 3339 instant. `send_dry_run` uses the same serializer and
returns the exact bytes and envelope (identical to what a later send produces) without writing a row or
opening a connection; the `Date` header is the draft's last-saved instant, so a draft sent later carries
that edit date. `outbox_list` reports every state, attempts, a **fixed** error sentence (stage and
code, never server text: a hostile SMTP/IMAP server can echo the password or the body), recipient count
and subject, plus any account stopped by an authentication failure. `limit` (1-500) and `offset` page
the newest-first history; terminal rows older than 30 days are pruned.

The old `draft_send` local move is gone from the registry. The CLI keeps `postino draft send <id>` as
an alias of `postino send --draft <id>`, and both now submit for real.

## Safety

- The server is opt-in and stdio-only; there is no network listener.
- The daemon pipe/socket is same-user and token-authenticated; a stale token is a readable refusal,
  never a fallback to a second writer. MCP never starts the daemon.
- Sending is off unless `postino mcp --allow-send`; `message_send`, `send_schedule`, `outbox_retry`,
  `outbox_mark_sent` and `rules_run` are refused before the arguments are read, an enable-only update
  of a stored forward rule is refused the same way, and a rule payload
  containing a forward action is refused before parsing. The server never runs the outbox pass: it
  only queues, and the daemon scheduler submits due rows. An uncertain submission is never retried
  automatically; a manual retry requires `confirm: true`.
- Tool arguments are validated against `deny_unknown_fields`; typos are rejected rather than
  ignored.
- Account credentials live only in the OS keyring. They are never in the database, a tool result, a
  log line or an MCP refusal; `account_add` and `account_remove` are not reachable over MCP at all.
- Never put credentials, tokens or message bodies in logs.

## Resources

The server advertises `resources` and supports `resources/list`, `resources/templates/list` and
`resources/read`. Resources are read-only and use the same registry operations as tools, so account
ownership and bounds are unchanged.

Static resources:

| URI | Content |
|---|---|
| `postino://mailbox/status` | Accounts plus their last sync status, never a credential |
| `postino://folders` | Folder ids, names, unread and total counts |
| `postino://messages/recent` | The newest 50 inbox summaries |

Templates:

| URI template | Content |
|---|---|
| `postino://message/{id}` | `message_get` JSON: body, recipients and attachment metadata |
| `postino://message/{id}/text` | Stored plain-text body |
| `postino://message/{id}/html` | Sanitized HTML body; plain-text mail is wrapped in `<pre>` |
| `postino://thread/{id}` | `thread_get` JSON: one conversation and its summaries |

A missing message or thread is `-32002`; a malformed id (including path-like or oversized values) is
`-32602`. The HTML variant returns only the sanitizer's output, never a remote fetch or a script.
