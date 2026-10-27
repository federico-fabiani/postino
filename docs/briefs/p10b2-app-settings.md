# Brief — P10b2 app tray settings and activation

> **Package:** P10b2 (app + the small daemon/CLI glue it needs). **Depends on:** P10a2, P10b1.
> **Parallel:** P11a1 (pure renderer in `crates/mime`; no app.rs). **Review:** one blind UX +
> single-writer review, one fix round.

## Outcome

The app exposes the P10b1 tray/notifier/autostart state in Settings, closes to tray only when the
person opted in and a daemon owns the store, and opens the outbox from the tray. No dead control: a
switch that cannot work in the current mode is visibly disabled with a reason.

## Read first

`AGENTS.md`, `docs/briefs/p10b1-platform.md`, `docs/briefs/p10-daemon.md` §4, P10b1's
`crates/platform` traits, `apps/postino-daemon/src/tray_host.rs` and settings persistence,
`apps/postino-app/src/{app,boot,daemon,settings}.rs`, `docs/ui/plan.md`.

## Required

- Settings → Account/Appearance area: rows for **Tray**, **Notifiche**, **Anteprime**, **Avvio
  automatico**. State comes from the daemon/platform, never a cached bool. Writes are explicit and
  reflected only after confirmation; unavailable states say why (no daemon, no tray session,
  registry denied). Autostart on/off uses the P10b1 CLI/API or the same platform trait; no second
  registry path.
- Add `--open <outbox>` to the app launch parser; `Coda di invio` in the tray re-adds the menu item
  using that flag (P10b1 removed it because it was dead). The app opens the outbox after boot and
  focuses it. No shell, no URL, no external command.
- Close-to-tray: when tray is enabled and the daemon owns the store, closing hides the window and
  keeps the daemon running; otherwise it exits. The first hide shows a one-time notice with the way
  back; `Esci` from the tray exits explicitly. No hidden process without a tray.
- If there is no live daemon, the tray/close switches are disabled with “richiede il daemon” and the
  in-process app still exits normally. Never start a daemon implicitly.
- Keep one command table; new commands get ids/shortcuts/handlers there. No schema migration; no new
  dependency; no WebView2/P11a rendering in this package.

## Tests

App headless tests: settings rows enabled/disabled with reasons; save/cancel and read-back; close
hides only in daemon+tray mode; `--open outbox` parse and boot; activation after a restart;
980×600 light/dark settings; no dead button. Daemon/CLI tests for the settings/autostart bridge.
Package tests only; no full `cargo xtask ci`. One settings screenshot only because the UI changed.

## Report (max 15 lines)

What changed, files, package tests/result, limits, deviations.
