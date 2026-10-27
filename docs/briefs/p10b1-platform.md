# Brief — P10b1 daemon tray, autostart and notifications

> **Package:** P10b1 (no `apps/postino-app/**`). **Depends on:** P10a1. **Parallel:** P10a2.
> **Review:** one blind FFI/privacy/UX review, one fix round. **Do not** touch app settings or
> close-to-tray; those are P10b2.

## Outcome

The daemon owns a native Windows tray icon, a real per-user autostart switch and privacy-preserving
new-mail/send-failure notices. The app can stay closed and still sync, send scheduled mail and show a
tray. Every OS surface is behind a trait and has a headless recorder; CI never touches the real
desktop, registry startup key or notification area.

## Read first

`AGENTS.md`, `docs/briefs/p10-daemon.md` §4, `crates/platform`, `apps/postino-daemon/src/{main,lib,
state,schedule}.rs`, `crates/ipc/src/platform.rs` (the scoped-unsafe precedent), `Cargo.lock` and
`docs/decisions/0005-daemon-dependencies.md`.

## Split and ownership

- **P10b1 (this package):** `crates/platform/src/{tray,autostart,notify}.rs` plus one `win32` module;
  `apps/postino-daemon/src/tray_host.rs`, hooks in daemon `main/lib/state/schedule`; CLI
  `postino daemon autostart status|on|off`; icon asset and ADR-0006. No `apps/postino-app/**`.
- **P10b2 (later):** app settings rows, close-to-tray and activation; do not implement it here.

## Decisions (accepted)

- **Tray owner is the daemon.** `Shell_NotifyIcon` on a daemon-owned HWND and message loop; menu
  `Apri Postino`, `Sincronizza ora`, `Coda di invio`, `Esci`. `Apri` launches the sibling app with
  `--store <store>` and optionally `--open outbox`; no shell and no remote command.
- **No new dependency.** Use the pinned `windows-sys 0.61.2` with the required features:
  `Win32_UI_Shell`, `Win32_UI_WindowsAndMessaging`, `Win32_Graphics_Gdi`,
  `Win32_System_LibraryLoader`, `Win32_System_Registry`. Reject `tray-icon`, `muda`,
  `notify-rust`, `winreg`, `auto-launch` and the `windows` crate.
- **Autostart = `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` value `Postino`**, per-user, no
  admin, reversible, readable back from the registry. This deliberately replaces the brief's Task
  Scheduler: document the deviation and why (COM-free, smallest real implementation). Tests use a
  parameterised key under `HKCU\Software\Postino\Tests\<pid>`, never the real Run key.
- **Notification = `Shell_NotifyIcon` `NIF_INFO` balloon**, title/body bounded and newline-stripped.
  With previews off, the payload contains no subject, sender or body. A coal/ivory icon state reflects
  quiet/sync/offline/failed; no badge noise. `TaskbarCreated` re-adds the icon.
- **Persistence:** `%APPDATA%\Postino\daemon.json` (atomic replace, `deny_unknown_fields`) with
  `tray`, `notify_enabled`, `notify_previews` (default false), `debounce_secs >= 60`. The new-mail
  watermark and last failed outbox id live in encrypted `meta`, so restart never repeats a notice.
- **Privacy:** no body/address/subject in logs; notifications are one per debounce window and one per
  message id; failure text is one fixed sentence with the outbox action.
- **Failure modes:** no Explorer/session 0 → one log line, tray disabled, daemon serves; registry
  denied → “autostart non disponibile”, state unchanged; icon re-add failure → retry on
  `TaskbarCreated`; store read failure → skip notice, counters only.
- **Console:** the daemon is autostarted at login, so it must not flash a console. Use
  `windows_subsystem = "windows"` for the release binary and a bounded log file when stderr is
  absent; keep debug/test output usable.

## Seams and safety

- Traits `Tray`, `Autostart`, `Notifier`; recorder implementations injected through daemon config.
  Win32 lives in one module with `#![allow(unsafe_code)]` scoped there; the crate keeps
  `unsafe_code = "deny"`. Every FFI call has a documented invariant; no unwrap/expect/panic outside
  tests; all strings/limits bounded.
- Icon asset: derive `assets/app-icon/postino.ico` from the existing `postino-256.png`, add its
  `ATTRIBUTION.toml` entry and SHA-256; `cargo xtask assets` must pass. No Microsoft binary asset.

## Tests and acceptance

- Recorder tests: menu actions, icon state transitions, preview-off payload has no PII, debounce and
  watermark dedupe, failure notice, autostart on/off/read-back, registry-denied mode, no Explorer.
- One real Win32 probe behind `POSTINO_WIN32_UI_TESTS=1`, never CI; daemon still serves when the tray
  cannot start.
- `cargo test -p postino-platform -p postino-daemon -p postino-cli` targeted; no full `cargo xtask ci`.
  Judge `test result:`; if piping, end with `; exit $LASTEXITCODE`.
- No screenshots for non-visual changes; a single 980×600 settings shot belongs to P10b2.

## Report (max 15 lines)

What changed, files, package tests and result, dependency/ADR decision, open risks, deviations.
