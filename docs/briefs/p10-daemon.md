# Brief — P10 daemon, single writer and background life

> **Package:** **P10a** (IPC + daemon + scheduler + clients), then **P10b** (tray, autostart,
> notifications, packaging seams). **Depends on:** P7 (outbox), P9a/P9b/P9c (search, threads, rules),
> P8 (compose) · **Next:** P11 renderer
> **Owner:** developer agent · **Review:** blind security + crash/identity + UX
>
> M3 ends with a complete client. M4 makes it live with the window closed: one process owns the store,
> the network and the scheduler; the app, CLI and MCP become clients of the same operations over a
> same-user channel. “Daemon not running” must be a normal, explained state, not a crash.

## Outcome

- A headless `postino-daemon` owns the encrypted store, sync, the outbox scheduler, local rules, the
  search-index batches and blob GC. It starts at login only when the person opts in, or on demand.
- The app, `postino` CLI and `postino mcp` speak one versioned JSON-RPC protocol to the daemon over a
  Windows named pipe or Unix domain socket, authenticated by a per-launch token.
- Scheduled mail sends with the app closed; a daemon restart loses no outbox row; two clients can
  mutate one store through the daemon without two writers.
- When the daemon is absent, every client shows a readable state and the documented fallback. No
  silent second writer, no dead command, no port, no remote listener.

## 1. Process model and single writer

- One daemon per user and store. It holds the only SQLite connection used for writes, the IMAP/SMTP
  sessions, the outbox lease, the rule runner and the index batches. Clients never open the store for
  writes while the daemon is alive.
- Daemon discovery: a well-known per-user endpoint name derived from the user SID (Windows) or UID
  (Unix) and the store path hash; a small state file records protocol version, pid, start instant and
  endpoint. Stale files are detected by a failed connect and removed atomically.
- Fallback matrix:
  - **App:** if the daemon is absent, it may run in-process exactly as today, with a visible
    “Solo questa finestra” state. Starting the daemon requires an explicit action; the app then
    reconnects and must not keep a second writer. If the daemon is present, the app is read/write
    through IPC only.
  - **CLI:** read-only commands may open the store directly when the daemon is absent (no writer
    conflict); mutations either start the daemon or refuse with “avvia il daemon”. Pick one and
    document it; do not guess per command.
  - **MCP:** never starts a background process implicitly. It connects when the daemon is present and
    otherwise returns a readable tool error for mutations; reads may use the cache directly.
- The store has a single-writer guard independent of SQLite (lock file with pid/start token, released
  on crash by a stale check). `postino --store` against a daemon-owned store is refused with a reason.
- Crash recovery: daemon restart re-leases `sending` rows to `uncertain` (never resends), keeps every
  queued/scheduled/append_pending row, rebuilds the thread/search caches only if needed, and logs one
  fixed sentence.

## 2. IPC protocol

- JSON-RPC 2.0 over a Windows named pipe / Unix domain socket, same user only. No TCP port, no
  remote address, no web socket.
- Handshake: `hello {protocol, client, token}`; the daemon verifies the peer is the same user (pipe/
  socket ACL plus process check where available), a fresh random token written to an owner-only file
  (Windows ACL, mode 0600 Unix), and a client protocol version. A bad token is a generic refusal with
  no detail; a stale token rotates on daemon start.
- Methods mirror the operation registry: `op {name, args}` plus `daemon_status`, `daemon_shutdown`,
  `events_subscribe` and `cancel {request}`. Responses use the same JSON Schemas and error shape as
  MCP/CLI; `isError`-style readable failures. Long operations emit bounded progress notifications
  (sync folder/message counts, index x/y, outbox state) and accept one cancellation token.
- Limits: 16 MiB request/response framing, bounded queues, one in-flight mutation per client, global
  concurrency caps, timeouts, heartbeat/ping, no unbounded subscription backlog. Every string is
  bounded, every request is `deny_unknown_fields` before dispatch.
- Logs: no body, subject, address, query, credential, token or path beyond the store root; structured
  counters only. The token file is never logged.

## 3. Scheduler

- Sync: on daemon start, on demand, and on an interval with jitter; no parallel sync for one account.
  Keep the existing cancellation and status semantics. IDLE is optional and behind a capability
  check; do not claim it if not implemented.
- Outbox: the same P7 lease/backoff/append policy, now always running. A scheduled message fires with
  the app closed; a crash after `250` never resubmits; append retries independently.
- Rules: P9c runs after each successful sync (received mail only), plus manual. No server Sieve.
- Search index: one 500-row batch per tick while pending/building, so the UI sees progress; no open
  block; `search_status` is served by the daemon.
- Blob GC: a bounded, resumable sweep of unreferenced blobs after a successful sync and on idle; it
  must never delete a blob referenced by a row, a draft, an outbox snapshot or a forwarded attachment.
- Idle: wake on the next scheduled instant, not a busy loop; CPU target <0.5% idle and RSS <80 MiB
  headless on the Windows host (report the actual numbers).

## 4. Tray, autostart, notifications (P10b)

- Tray icon with menu: Apri Postino, Sincronizza ora, Coda di invio, Esci. The icon reflects a quiet
  state, sync, offline, uncertain/failed outbox; no badge noise. Closing the window hides to tray
  only if the person opted in; otherwise it exits.
- Autostart is opt-in, per user, reversible, and never installs a service: Task Scheduler (Windows),
  LaunchAgent (macOS), `systemd --user` (Linux). The settings page shows the exact command and the
  current state.
- Notifications: new mail and send failure; no body, sender address or subject by default unless the
  person enables previews. Clicking opens the app. Notifications are debounced and never repeat for
  the same message. A failure notification is one sentence with the outbox action.
- OS integration lives behind traits/seams (`Tray`, `Autostart`, `Notifier`) so headless tests use
  recorders. No shell, no native dialog, no notification in CI.
- Dependencies for tray/notifications/autostart need an explicit review (licence, size, platform
  support); do not pull a runtime or a browser engine into core.

## 5. Operations, CLI, MCP and app

- `daemon_status` (read-only): version, pid/start, store path (redacted to the user root if needed),
  accounts syncing, outbox next wake, index state, rules running, last error sentence. No secret.
- CLI: `postino daemon status|start|stop|logs?` (no `logs` that dumps content), existing commands work
  through the daemon when present; `--in-process` is an explicit diagnostic only.
- MCP: same tools; the transport is stdio as before, the MCP process is a client. No new listening
  port. Mutations fail readably without a daemon.
- App: connects through a `DaemonClient` trait; loading/offline/daemon-absent states are first-class;
  no dead “Riprova” when the daemon is not running; reconnect keeps selection, filters, open message
  and composer draft. Undo/toasts remain client-side where they are view state.
- One command table; no second operation list in the daemon. Schemas derive from the registry.

## 6. Tests

- IPC: handshake/token accept/refuse/rotate, wrong user, stale file, version mismatch, framing/queue
  bounds, cancellation, heartbeat, one in-flight mutation, unknown method, readable errors, no token
  in logs.
- Single writer: app + CLI against one daemon mutate one store; a direct store writer is refused; two
  daemons cannot own one store; daemon crash releases the lock; restart preserves outbox/rules/index.
- Scheduler: scheduled send with the app closed; daemon restart at every P7 crash point; append retry;
  rules run once after sync; index batches; blob GC leaves every referenced blob and removes only
  unreferenced ones.
- Clients: app fallback/reconnect preserves view state; CLI/MCP absent-daemon matrix; read-only
  commands; no second writer.
- Tray/autostart/notifications: recorder seams, menu actions, opt-in state, notification privacy and
  debounce, no OS integration in CI.
- Performance: idle CPU/RSS, two-client latency, sync with the app closed, 100k open/search budgets
  from P9a (the daemon must not add a body load at boot).
- Security: same-user only, token file ACL, no remote listener, no secret/PII in logs, bounded
  payloads, cancellation cannot bypass an operation.

## 7. Docs

- Architecture process model, IPC handshake, scheduler, fallback matrix, single-writer lock, GC.
- Threat model: token file, same-user pipe/socket, malformed frames, denial of service, notification
  privacy, autostart command, daemon shutdown.
- README quick start: daemon status/start, autostart opt-in, app-closed send, “daemon not running”.
- `docs/mcp.md`: daemon client behavior, absent-daemon mutations, no port.
- `docs/development.md`: headless IPC fixture, recorder seams, crash tests, platform notes.
- ROADMAP M4; delivery plan P10a/P10b split.

## Out of scope

A remote or multi-user daemon, a web app, server-side filters/Sieve, OAuth, PGP, HTML rendering,
mobile, a Windows service, a browser runtime in the daemon, auto-update, telemetry.

## Constraints

- No new dependency without a written licence/size/platform review; core/store stay OS-agnostic.
- No unsafe outside the minimal platform adapter, no unwrap/expect/panic outside tests, bounded
  frames/queues, no input-derived slicing, no unbounded subscription.
- Same-user only, token never in argv/logs, no TCP listener, no cloud.
- Rustfmt 150, clippy `-D warnings`, `cargo xtask ci`; temp only `%TEMP%\opencode\p10-postino`.

## Gates

`cargo xtask ci` green; two clients mutate one store; scheduled mail sends with the app closed;
daemon restart at every P7 point loses nothing and never resends; token/auth/log tests; fallback
matrix; tray/autostart/notification recorder tests; idle CPU/RSS reported; blind security and UX
review with no blocker.

## Report

Process/IPC design, token/ACL evidence, scheduler/GC evidence, two-client transcript, crash/restart
matrix, fallback matrix, tray/autostart/notification seams, perf numbers, tests, docs, deviations.
