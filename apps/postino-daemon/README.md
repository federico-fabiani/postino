# postino-daemon

The headless owner of one Postino store. It opens the SQLCipher database with the single-writer lock,
builds the `Engine`, binds the per-user pipe/socket and runs the scheduler. The desktop app, the
`postino` CLI and `postino mcp` are clients of the same operation registry over `postino-ipc`.

## What it owns

- The one write connection and the `<store>.lock` / `<store>.owner` pair.
- Sync on start, on a jittered interval and on a manual `op sync_now`; no parallel sync for one
  account. A sync is split into message-bounded chunks, so the engine is released between chunks and
  the outbox/index can run during a long initial sync.
- The P7 outbox pass: due rows, leases, Sent append and backoff. An expired `sending` lease becomes
  `uncertain` and is never resent; a queued/scheduled row is kept across a restart.
- P9c local rules after each successful sync (received mail only).
- P9a search-index batches: one 500-row batch per tick while pending, with `index` progress events.
- Bounded, resumable blob GC after a successful sync and on its timer; it rechecks every attachment
  and outbox snapshot reference before unlinking.
- The local JSON-RPC server: `op`, `ops_list`, `daemon_status`, `daemon_shutdown`,
  `events_subscribe` and `cancel`. No TCP listener, no remote endpoint.

It does **not** own the tray, autostart or notifications: those are P10b and are not implemented here.
The app still runs in-process in P10a1 and refuses to boot when this daemon owns the store; P10a2
connects it.

## Run it

```sh
cargo build -p postino-daemon -p postino-cli
postino daemon start
postino daemon status
postino daemon stop
```

The binary takes `--store <path>`; without it the default is `<data dir>/mail.db`. The store key and
account credentials come from the OS keyring, never from argv or a file. `--sync-interval`,
`--gc-interval` and `--no-sync-on-start` tune the scheduler; they are not part of the stable client
surface.

## Tests

The library half is what the tests use:

- `cargo test -p postino-daemon` starts real daemons on temporary stores with a fixed key and an
  in-memory vault. It covers two clients on one store, scheduled send with no client, index batches,
  an outbox row due during a long sync, blob GC, rules after sync, MCP send gating, a disconnect and a
  shutdown during sync, token rotation, idle wake count, no TCP listener and a capture logger with no
  token/address/subject/body.
- `cargo test -p postino-daemon --test restart_crash` kills a real child daemon at every P7 outbox
  crash state and starts a replacement.
- `cargo test -p postino-cli --test daemon_client` drives the real CLI binary and the MCP stdio server
  against the daemon, including `daemon status/start/stop`, the `--in-process` refusal, the 16 MiB
  attachment/dry-run refusals and the absent-daemon mutation policy.

No test touches `%LOCALAPPDATA%\Postino` or the real keyring.
