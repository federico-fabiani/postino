# Brief — P10a2 app daemon client

> **Package:** P10a2 (app client; MCP/CLI already clients in P10a1). **Depends on:** P10a1.
> **Review:** blind architecture + crash/identity. **Do not** add tray, autostart or notifications.

## Outcome

The desktop app connects to the live daemon over the existing `postino-ipc` client instead of opening a
second writer. With no daemon it keeps the documented in-process fallback, visibly labelled, and can
start/stop the daemon only on an explicit user action. A daemon restart never loses the selected
folder, filters, open message, thread view or composer draft.

## Read first

`AGENTS.md`, `docs/briefs/p10-daemon.md` §1–2 and §5, `crates/ipc/README.md`,
`apps/postino-daemon/README.md`, `apps/postino-app/src/boot.rs`, `sync_worker.rs`, `app.rs`,
`apps/postino-cli/src/mcp.rs`.

## Required behaviour

- **Single writer.** If the daemon owns the store, the app must not acquire `WriterLock` or open a
  writable `Engine`. Reads may use one documented path (read-only store or daemon `op`); writes and
  network work go through IPC only. `postino --store` and the app must refuse a second writer with
  the existing fixed sentence.
- **Fallback.** No daemon means in-process mode exactly as today, with a visible “Solo questa
  finestra” state. Never start a daemon silently. An explicit “Avvia daemon” action may start it,
  after which the app reconnects and releases its writer lock. “Ferma daemon” is explicit.
- **Reconnect.** Connection loss is a normal state. Track reconnect with bounded backoff; preserve
  all view state and the composer draft. Do not leave a dead “Riprova”; after a failed reconnect
  offer the in-process fallback only when it is safe and explicit.
- **Events.** Subscribe to the daemon's bounded events for sync, outbox, rules and index; refresh the
  same store-backed views the app refreshes today. No polling loop beyond the existing tick.
- **Sensitive operations.** `account_add`/`account_remove` remain available from the app over the
  same-user IPC (the daemon already admits the `app` client; MCP stays excluded). The password is
  never logged, never in argv, never written to disk. Onboarding must keep working in both modes.
- **Demo.** `--demo` remains synthetic and offline; no daemon connection, no writer, no IPC.
- **CLI/MCP.** Do not regress P10a1: live daemon clients, read fallback, mutation refusal, per-session
  `--allow-send`, no implicit daemon start from MCP.
- **One registry.** No second operation table; names/schemas/annotations come from `postino-core`.

## Tests and evidence

Targeted app tests for: daemon present → no writer; daemon absent → in-process + label; explicit
start/stop; kill daemon → readable offline state; restart → reconnect preserves folder/filter/open
message/thread/composer; sensitive onboarding over IPC; no secret in logs; 980×600 light/dark.
A two-process transcript (daemon + app) is required. Package suites only; no full `cargo xtask ci`.

## Constraints

No schema change, no new dependency without a written review, no TCP listener, no secret or token in
logs, no unwrap/expect/panic outside tests, bounded reconnect and payloads. Keep the existing UI
command table and visual language.

## Report (max 15 lines)

What changed, files, package tests and result, open risks, deviations. No code or log dumps.
