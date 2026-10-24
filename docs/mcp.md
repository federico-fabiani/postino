# MCP

Postino exposes the same operation registry as the app and the CLI through an opt-in MCP server.

## Start it

```sh
postino mcp
postino --demo mcp
```

- Transport: newline-delimited JSON-RPC 2.0 on stdin/stdout.
- It opens no port, starts nothing else, and stops at EOF.
- Logs, if any, go to stderr. stdout carries protocol messages only.
- Without `--demo`, the engine has no accounts until the M1 store exists; tool calls return empty
  lists or a readable error.

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

## Tools (M0)

| Tool | Read-only | Purpose |
|---|---|---|
| `app_info` | yes | Version, platform, data directory, demo flag, operation count |
| `accounts_list` | yes | Configured accounts |
| `folders_list` | yes | Folders with unread and total counts |
| `messages_list` | yes | Message summaries, paginated; `label` alone spans every folder, `folder` narrows it |
| `message_get` | yes | One message with body and attachments |
| `message_flag` | no | Set read/unread and/or starred |
| `message_move` | no | Move a message to a regular folder |
| `draft_save` | no | Create or update one draft in place |
| `draft_send` | no | Demo only: move a draft to Sent, with no SMTP connection |
| `search` | yes | Case-insensitive search over subjects, people and bodies, with optional folder and label filters |
| `labels_list` | yes | Labels with mailbox-wide total and unread counts |
| `label_create` | no | Create a local label with a semantic colour |
| `label_update` | no | Rename and/or recolour a label; the id is stable |
| `label_delete` | no | Delete a label, detach it from every message, return the affected count |
| `label_apply` | no | Apply or remove one label on 1–500 messages; idempotent |

Annotations:

- `readOnlyHint`: true for reads and `labels_list`; false for every other write.
- `destructiveHint`: false in M0. `label_delete` removes metadata only, never mail. Sending and
  deletion will be marked destructive in M6.
- `idempotentHint`: true for reads, `message_flag`, `label_apply` and `label_update`.
- `openWorldHint`: false. The server talks to the local engine, not the open internet.

`labels_list`, `label_create`, `label_update`, `label_delete` and `label_apply` are backed by the
same registry the app and the CLI use. Labels are local metadata held in memory until the M1 store
exists; they are never sent to a server. Label ids are stable for the life of the label and never
reused, so an id from a deleted label is dead. Names are trimmed, at most 48 characters, unique
ignoring case, and refuse control, bidi and format characters (U+061C, U+200B–U+200F, U+202A–U+202E,
U+2060–U+206F, U+FEFF) that could reorder or hide text.

`messages_list` accepts an optional `label` id. With only `label` it returns every folder; with
`folder` and `label` it returns their intersection; with neither it defaults to `inbox`. An unknown
folder or label id is an error instead of an empty list. `search` also accepts `label` and applies it
before the row limit, so a match outside the first 200 rows is still returned. `label_apply`
validates every message id before changing anything, caps a call at 500 ids and returns how many
messages actually changed, so an agent can retry safely.

`draft_save` refuses CR/LF in addresses or the subject, validates every recipient, caps recipients
and attachments, and returns the draft with a stable id. `draft_send` requires at least one
recipient and is explicitly labelled demo: it moves the draft to Sent locally, without opening a
connection. Real submission, dry-run, rate limits and an audit log arrive with M2.

## Safety

- The server is opt-in and stdio-only; there is no network listener.
- The daemon pipe (M4) will be same-user and token-authenticated.
- `draft_send` is demo-only and never opens a connection. The real M2 send tool will add `dry_run`,
  idempotency keys, a rate limit and an audit log; recipient caps and header-injection checks are
  already in place. See `docs/threat-model.md`.
- Tool arguments are validated against `deny_unknown_fields`; typos are rejected rather than
  ignored.
- Never put credentials, tokens or message bodies in logs.

## Resources (planned)

M6 will expose read-only resources for open threads and messages, for example
`postino://thread/{id}` and `postino://message/{id}/html`, plus a compact tool mode for agents with
small context budgets.
