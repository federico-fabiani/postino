# MCP

Postino exposes the same operation registry as the app and the CLI through an opt-in MCP server.

## Start it

```sh
postino mcp
postino --demo mcp
postino --store <path> mcp
```

- Transport: newline-delimited JSON-RPC 2.0 on stdin/stdout.
- It opens no port, starts nothing else, and stops at EOF.
- Logs, if any, go to stderr. stdout carries protocol messages only.
- Without `--demo` and `--store`, `postino mcp` opens the default encrypted store at
  `%LOCALAPPDATA%\Postino\mail.db` on Windows. Windows Credential Manager is the only keyring backend
  in this build; macOS and Linux fail at the platform seam until their native keychains land with
  packaging, so there is no platform-equivalent default store yet. `--store` picks another database;
  one process owns it. `--demo` stays synthetic and never touches the store.
- The process needs the store key from the OS keyring. It never prints it.

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
| `message_flag` | no | Set read/unread and/or starred |
| `message_move` | no | Move a message to a regular folder |
| `draft_save` | no | Create a draft or patch one in place |
| `draft_send` | no | Move a draft to Sent locally; no SMTP connection in this build |
| `search` | yes | Case-insensitive search over subjects, people and bodies, with optional folder and label filters |
| `labels_list` | yes | Labels with mailbox-wide total and unread counts |
| `label_create` | no | Create a local label with a semantic colour |
| `label_update` | no | Rename and/or recolour a label; the id is stable |
| `label_delete` | no | Delete a label, detach it from every message, return the affected count |
| `label_apply` | no | Apply or remove one label on 1–500 messages; idempotent |

## Sensitive operations

`account_add` and `account_remove` are marked `sensitive` in the operation registry. They are **not**
in `tools/list`, and `tools/call` refuses them before it parses the arguments, with a readable
`isError` result that never repeats the arguments. The reason is structural, not a policy: MCP cannot
see or set a password, and it cannot create or remove an account. The same registry entries remain
available to the CLI (`postino account add|remove`), whose password path is a no-echo prompt or one
`--password-stdin` line; there is deliberately no `--password` argv flag, and `postino run` refuses
sensitive operations so generic JSON arguments cannot carry a password into shell history.
`account_test` is not sensitive because it never receives a password: it uses the credential already
in the OS keyring.

Annotations:

- `readOnlyHint`: true for `accounts_list`, `account_test`, `sync_status` and every read.
- `destructiveHint`: true for `account_remove` (it deletes the local cache for one account). Sending
  and message deletion will be marked destructive in later milestones.
- `idempotentHint`: true for reads, `message_flag`, `label_apply` and `label_update`.
- `openWorldHint`: true for `account_test` and `sync_now`, which open a connection to the user's own
  IMAP server. Everything else is local.

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

`draft_save` refuses CR/LF in addresses or the subject, validates every recipient, caps recipients
and attachments, and returns the draft with a stable id. **It is a patch on update:** every content
field (`to`, `cc`, `bcc`, `subject`, `body_text`, `attachments`) is optional; omitting a field keeps
its current value and sending it — even as an empty list or empty string — replaces it. On create,
omitted fields are empty, and `account` chooses the account; it is required when more than one
account exists. The app sends the full content on every autosave, which is equivalent to replacing
every field. `draft_send` requires at least one recipient, moves the draft to the Sent folder of its
own account, and opens no connection: the move is local and durable on a `--store` database, and
real submission arrives with M2.

## Safety

- The server is opt-in and stdio-only; there is no network listener.
- The daemon pipe (M4) will be same-user and token-authenticated.
- `draft_send` never opens a connection and is honestly labelled "Move a draft to Sent (local)".
  The real M2 send tool will add `dry_run`, idempotency keys, a rate limit and an audit log;
  recipient caps and header-injection checks are already in place. See `docs/threat-model.md`.
- Tool arguments are validated against `deny_unknown_fields`; typos are rejected rather than
  ignored.
- Account credentials live only in the OS keyring. They are never in the database, a tool result, a
  log line or an MCP refusal; `account_add` and `account_remove` are not reachable over MCP at all.
- Never put credentials, tokens or message bodies in logs.

## Resources (planned)

M6 will expose read-only resources for open threads and messages, for example
`postino://thread/{id}` and `postino://message/{id}/html`, plus a compact tool mode for agents with
small context budgets.
