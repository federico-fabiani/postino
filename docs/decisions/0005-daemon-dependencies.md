# ADR-0005 — daemon and IPC dependencies

- **Status:** accepted
- **Date:** 2026-10-27
- **Scope:** P10a1 (`postino-ipc`, `postino-daemon`) and the CLI/MCP client

## Decision

P10a adds **no new third-party crate version**. It uses entries already in `Cargo.lock` and the
workspace, declared directly where the source needs them:

| Dependency | Licence | Size / platform | Why direct |
|---|---|---|---|
| `tokio` + `sync`, `rt-multi-thread` features | MIT | existing workspace crate; features are code already in the link graph | `mpsc`, `broadcast`, `Semaphore`, `Notify` for bounded queues and the scheduler; the daemon runtime has two worker threads |
| `windows-sys = 0.61.2` (foundation, security, authorization, storage-file-system, threading, system-services) | MIT OR Apache-2.0 | already in the lockfile via `windows-native-keyring-store`; only the named-pipe DACL, current-SID, atomic replace and link-count calls are enabled | the Windows ACL adapter in `crates/ipc/src/platform.rs` and the store's hard-link check |
| `libc = 0.2` (Unix only) | MIT OR Apache-2.0 | already in the lockfile via the keyring stack; one `getuid` call | stable per-user endpoint names on Unix |
| `fs4 = 0.13` (store) | MIT OR Apache-2.0 | already in the workspace and lockfile (used by `postino-platform`) | the OS writer lock on `<store>.lock` |
| `blake3`, `getrandom`, `serde`, `serde_json`, `thiserror`, `log`, `zeroize` | existing workspace | already used by core/store/platform | endpoint hashing, token generation, protocol types |
| `clap`, `env_logger`, `anyhow` | existing workspace | already used by the CLI | the daemon binary's small argument surface |

No tray, notification, autostart, webview, HTTP or async-runtime dependency is added for P10a1. The
daemon has no `egui` dependency; `cargo xtask layers` enforces that.

## Consequences

- The Windows build gains no new crate version; the `windows-sys` feature set grows only for the ACL
  and SID calls. The Unix build gains no new crate version; `libc` was already in the graph.
- `tokio::sync` and `rt-multi-thread` are additive features of an existing crate. The core and store
  still do not require them to link; the daemon binary is the only current-thread/multi-thread runtime
  owner.
- `fs4` moves from a `postino-platform`-only dependency to `postino-store`, where the lock belongs.
  The workspace `Cargo.toml` already pins the same version.
- The one `unsafe` is the documented Windows ACL/SID adapter in `postino-ipc`; the crate's lint is
  `unsafe_code = "deny"` and the module opts in explicitly with safety comments. Every other crate
  keeps the workspace `forbid`.
