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
| `messages_list` | yes | Message summaries, paginated, filterable |
| `message_get` | yes | One message with body and attachments |
| `message_flag` | no | Set read/unread and/or starred |
| `message_move` | no | Move a message to a regular folder |
| `draft_save` | no | Create or update one draft in place |
| `draft_send` | no | Demo only: move a draft to Sent, with no SMTP connection |
| `search` | yes | Case-insensitive search over subjects, people and bodies |

Annotations:

- `readOnlyHint`: true for reads; false for `message_flag`, `message_move`, `draft_save` and
  `draft_send`.
- `destructiveHint`: false in M0. Sending and deletion will be marked destructive in M6.
- `idempotentHint`: true for reads and for `message_flag`.
- `openWorldHint`: false. The server talks to the local engine, not the open internet.

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
