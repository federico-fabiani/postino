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
- The Windows tray, its three P10b1 menu actions and new-mail/send-failure notices. The tray is one
  `Shell_NotifyIcon` icon state (quiet, syncing, offline, failed) and never a badge count. Menu
  actions open the sibling app with `--store`, request a sync or stop the daemon. A missing
  notification area is one log line; the daemon keeps serving.
- The opt-in per-user autostart value (`HKCU\...\Run\Postino`) written and read back by
  `postino daemon autostart on|off|status`. No service and no elevation; ADR-0006 records the
  registry-Run choice.
- `%APPDATA%\Postino\daemon.json`: `tray`, `notify_enabled`, `notify_previews` (default false) and
  `debounce_secs` (minimum 60), written atomically with `deny_unknown_fields`. Watermarks live in
  the encrypted store `meta`, so a restart never repeats an old notice. Release builds have no
  console and write a bounded `%APPDATA%\Postino\daemon.log` when stderr is absent.

The app still runs in-process in P10a1 and refuses to boot when this daemon owns the store; P10a2
connects it. App settings rows and close-to-tray remain P10b2.

## Run it

```sh
cargo build -p postino-daemon -p postino-cli
postino daemon start
postino daemon status
postino daemon autostart status
postino daemon autostart on
postino daemon stop
```

`autostart on` stores the exact quoted daemon and store command for this user; `off` removes it and
`status` reads it back. Settings live in `%APPDATA%\Postino\daemon.json`.

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
- `cargo test -p postino-daemon --test tray` exercises tray menu actions, the app launcher recorder
  and a failing tray through the real daemon; no test opens a notification area.
- `cargo test -p postino-platform` covers the recorder seams, notification privacy/debounce,
  autostart quoting and a throwaway `HKCU\Software\Postino\Tests` round trip. The real
  `Shell_NotifyIcon` probe runs only with `POSTINO_WIN32_UI_TESTS=1` and never in CI.
- `cargo test -p postino-cli --test daemon_client` drives the real CLI binary and the MCP stdio server
  against the daemon, including `daemon status/start/stop`, the `--in-process` refusal, the 16 MiB
  attachment/dry-run refusals and the absent-daemon mutation policy.

No test touches `%LOCALAPPDATA%\Postino` or the real keyring.
