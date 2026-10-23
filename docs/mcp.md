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
| `search` | yes | Case-insensitive search over subjects, people and bodies |

Annotations:

- `readOnlyHint`: true for every tool except `message_flag`.
- `destructiveHint`: false in M0. Sending and deletion will be marked destructive in M6.
- `idempotentHint`: true for reads and for `message_flag`.
- `openWorldHint`: false. The server talks to the local engine, not the open internet.

## Safety

- The server is opt-in and stdio-only; there is no network listener.
- The daemon pipe (M4) will be same-user and token-authenticated.
- Future send tools will support `dry_run`, idempotency keys, a recipient cap, a rate limit and an
  audit log. See `docs/threat-model.md`.
- Tool arguments are validated against `deny_unknown_fields`; typos are rejected rather than
  ignored.
- Never put credentials, tokens or message bodies in logs.

## Resources (planned)

M6 will expose read-only resources for open threads and messages, for example
`postino://thread/{id}` and `postino://message/{id}/html`, plus a compact tool mode for agents with
small context budgets.
