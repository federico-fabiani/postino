# ADR-0006: daemon-owned Windows tray, autostart and notifications

- Status: accepted
- Date: 2026-10-27
- Scope: P10b1 (`postino-platform`, `postino-daemon`, `postino-cli`); P10b2 adds app activation

## Context

P10b needs a tray icon, an opt-in per-user autostart switch and new-mail/send-failure notices while
the app is closed. The P10 brief proposed Task Scheduler for autostart and left the tray implementation
open. The workspace rule is no new third-party dependency without a written review; the daemon already
depends on pinned `windows-sys 0.61.2` and must keep `unsafe` inside one platform module.

## Decision

- **Tray and notices** use `Shell_NotifyIconW` on a hidden window owned by the daemon. The window
  thread parses a bounded recorder channel; the P10b1 menu is `Apri Postino`, `Sincronizza ora`,
  `Esci`. `Apri` launches the sibling `postino-app` with `--store <store>`, never a shell. An
  outbox entry needs app activation and belongs to P10b2, so P10b1 does not ship a dead menu item.
  `NIF_INFO` balloons carry a fixed title and body when previews are off (the default); a
  previews-on notice is bounded to one sender and one subject and is never logged.
- **Autostart is the per-user registry Run value** `HKCU\Software\Microsoft\Windows\CurrentVersion\Run\Postino`,
  containing `"<daemon>" --store "<store>"`. This replaces the brief's Task Scheduler. The Run key is
  COM-free, needs no elevation and no service, is reversible with one value delete, and can be read
  back exactly, which the settings page and `postino daemon autostart status` require. Task Scheduler
  would add COM, a task definition to migrate and an admin-adjacent surface for the same login start.
  Documented limits: it runs only when this user logs in (never at boot or for another user), has no
  restart policy and no delay, and Explorer may start it before the shell is ready; the daemon handles
  that as a normal no-tray state.
- **The platform crate owns every OS call** behind `Tray`, `Autostart` and `Notifier`. Recorder
  implementations are injected through `DaemonConfig`; CI never opens a notification area, writes the
  real Run key or starts a GUI. Registry tests use `HKCU\Software\Postino\Tests\<pid>-<counter>` and
  delete that subtree.
- **Dependencies stay at zero new crate versions.** `windows-sys 0.61.2` gains only the
  `Win32_UI_Shell`, `Win32_UI_WindowsAndMessaging`, `Win32_Graphics_Gdi`, `Win32_System_LibraryLoader`
  and `Win32_System_Registry` features already present in the lockfile. `tray-icon`, `muda`,
  `notify-rust`, `winreg`, `auto-launch` and the `windows` crate are rejected.
- **Settings persist to `%APPDATA%\Postino\daemon.json`** with an atomic replace and
  `deny_unknown_fields`: `tray`, `notify_enabled`, `notify_previews` (default `false`) and
  `debounce_secs` (minimum 60). The new-mail horizon is one bounded scalar in the encrypted store
  `meta` table (the newest announced message date, written only after the OS accepted a notice) and
  the last failed outbox id is stored beside it, so a restart never repeats a notice.
- **Release binary** uses `windows_subsystem = "windows"` and, when stderr is absent, a 1 MiB rotating
  `%APPDATA%\Postino\daemon.log`; debug builds keep stderr for tests and diagnostics.

## Consequences

- One `unsafe` module (`crates/platform/src/win32.rs`) with scoped `allow(unsafe_code)`; the crate
  keeps `deny`, and every FFI call documents its invariant.
- A missing Explorer/notification area, denied registry write or store read failure degrades to one
  log line and a serving daemon; it never panics and never blocks IPC.
- The tray state has four values (quiet, syncing, offline, failed) and no badge counts. Notification
  policy is one notice per debounce window and one per message/outbox id.
- If login-start reliability becomes a product requirement beyond the Run key (boot start, service,
  restart policy), a later ADR must choose Task Scheduler or a service explicitly.
