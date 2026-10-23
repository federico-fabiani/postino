# ADR-0002 — one daemon owns the store, sync and scheduler

- **Status:** accepted
- **Date:** 2026-10-23

## Context

Postino has three frontends (app, CLI, MCP) and background work (IMAP IDLE, scheduled send, local
rules). If each process opens the SQLite database and starts its own sync loop, we get:

- two writers and lock contention;
- duplicate IMAP connections and duplicate flag updates;
- drafts that race between the GUI and an agent;
- scheduled mail that only sends while the app happens to be open;
- no single place to audit what an agent did.

## Decision

`postino-daemon` is the only process that owns the store, the network connections and the
scheduler. The app, CLI and MCP server are clients.

- Transport: JSON-RPC 2.0 over a Windows named pipe or a Unix domain socket.
- Authentication: a fresh random token per daemon launch, written to an owner-only file; the pipe
  accepts the same user only.
- Lifecycle: starts at login (Task Scheduler / LaunchAgent / `systemd --user`), lives in the tray,
  restarts on crash. The CLI starts it on demand if it is not running.
- MCP keeps its stdio transport; its process is a client of the daemon, not a second engine.
- M0 runs in-process because there is no store or network yet. The `Engine` API is the seam that the
  daemon will own; moving it behind IPC must not change operation names or schemas.

## Consequences

- One writer, one sync loop, one scheduler, one audit log.
- The app can close without stopping mail sync.
- A daemon is an extra process to install, supervise and debug. It needs a clean crash story and a
  visible status in the UI.
- The app must handle "daemon not running" and "daemon restarting" as normal states, not fatal
  errors.
- A future headless server or Docker image can reuse the same daemon without the GUI.
