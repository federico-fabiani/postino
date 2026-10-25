# Brief — P6b onboarding, real mailbox and sync UI

> **Package:** P6b · **Depends on:** P6a (accounts + sync operations) · **Next:** P7 SMTP
> **Owner:** developer agent · **Review:** blind UX + blind engineering
>
> The first release a person can actually use on their own mail. The demo must become a fallback,
> not the product.

## Outcome

- First run: a guided account setup for Aruba, Gmail or a custom IMAP server, with a real connection
  test, clear errors and a first sync with progress and cancel.
- Later runs: the app opens the encrypted store, shows cached mail immediately, syncs in the
  background, and tells the truth about offline, syncing, stale and error states.
- The reading pane shows real messages with their attachments; Save works. Compose and drafts work on
  the real account; send stays a local move with the existing notice until P7.
- Account switch, add and remove live in the UI. Settings persist. No dead actions.

## 1. Boot and worker

- `postino-app` chooses the store at the standard path (same default as P6a) unless `--demo` is
  passed. The demo remains available and is labelled.
- `Engine::open` is fallible: the store/key failure screen explains the path, the keyring and what to
  do, with "Riprova" and "Apri la modalità demo"; it never panics and never silently starts empty.
- Sync runs on one background worker thread owning its own store connection, with a command channel
  (`SyncNow(account)`, `Cancel`, `Stop`) and event channel (`Started`, `Progress`, `Finished`,
  `Failed`). The UI never blocks on IMAP, never calls egui from the worker, and coalesces duplicate
  commands. A second sync request while one runs shows "Sincronizzazione già in corso".
- After a successful sync the engine reloads the account's messages from the store (or the worker
  returns the changed ids and the engine refreshes just those) without losing selection, filters or
  the open message.

## 2. Onboarding

Stages, each a real state with Back/Continue, not a fake wizard:

1. **Benvenuto** — one sentence, "Aggiungi il tuo account", "Esplora la demo".
2. **Provider** — Aruba, Gmail, Altro. Gmail shows the app-password/2FA note; Aruba shows where to
   generate an app password. "Altro" reveals host, port and TLS (implicit/STARTTLS).
3. **Credenziali** — display name, email, password (masked, paste works, Caps Lock hint), "Verifica
   connessione". The password is held zeroised in memory and never logged or persisted outside the
   keyring. Errors are specific: wrong password, app password required, TLS, DNS, timeout, server
   refused IMAP. Every error keeps the form filled and the focus on the field to fix.
4. **Cartelle** — the folders found, the mapped roles (Posta, Inviati, Bozze, Cestino, Spam,
   Archivio), warnings for missing roles; Continue creates the account locally (P6a operation).
5. **Prima sincronizzazione** — determinate progress (folders done / total, messages), a Cancel
   button that stops cleanly, "Continua in background" to enter the mailbox while it runs.
6. **Fatto** — the mailbox.

Keyboard: Tab order, Enter continues, Esc goes back with the existing unsaved guard, focus visible,
no password in the app title or accessibility labels. The screens must work at 980×600 and 1280×720,
light and dark.

## 3. Mailbox on real data

- Account switcher in the sidebar header: name, email, provider, unread total; switch, "Aggiungi
  account", "Gestisci account". Switching reloads folders, messages, labels and the sync status.
- Sidebar folders come from the store (special-use mapped), with counts from the store. No hardcoded
  demo folders when an account exists.
- Sync status in the toolbar/status bar: `Sincronizzazione…` with progress, `Aggiornato alle HH:MM`,
  `Offline · ultimo sync HH:MM`, `Errore · <reason>` with Retry. Refresh now is enabled and runs the
  worker; it replaces the disabled placeholder from P2b.
- Offline: cached messages and bodies stay readable; a banner says the app is offline, not that the
  mailbox is empty. A failed sync never clears the list.
- Search, labels, flags and moves keep working on the local cache. Flag edits stay local until P7:
  the message action shows that they are local-only once, in the same honest voice as send.

## 4. Reading pane

- Show the plain-text body (`text`, which is already the HTML-to-text alternative when a message has
  only HTML). A quiet "Vista testo" label explains that HTML rendering arrives with the sandboxed
  renderer; do not fake HTML.
- Header: sender, recipients, date, subject, label chips, attachment chips with filename, MIME and
  size. Remote images blocked count when the message had HTML.
- Links in the text: show the destination on hover; click asks before opening the system browser;
  only `http`/`https`/`mailto`, never `javascript:`, `file:`, `data:`.
- Attachments: click opens a Save dialog (rfd) with a safe default filename; write the blob bytes,
  never overwrite without confirmation, warn on executables and macro-enabled documents, show
  success/failure as a toast. Keyboard reachable. A missing/corrupt blob is a readable error, never a
  crash.
- Long messages scroll inside the pane; the pane keeps its 52% rule and the narrow overlay.

## 5. Compose and account identity

- New/reply/forward uses the active account's identity; the From line is visible and read-only with
  one account.
- Drafts save to the store and survive restart; send keeps the local-move notice and the no-SMTP
  tooltip. Attachments remain metadata-only until P7; the drop zone says so.

## 6. Settings

A dialog or page with: accounts (add, remove with the destructive confirmation and the local-cache
warning), theme, reduced motion, reading-pane width reset, store path (read-only display), version,
licences. Remove account is disabled with a reason when only one exists and it is the active one?
No: allow it, but explain the app returns to onboarding.

## 7. Tests and screenshots

- Headless onboarding: validation, each error class, success with the fake server (inject a sync
  service or a test config), cancel at each stage, keyboard-only path, 980×600.
- Boot: store with account → mailbox from store, no demo data; store open failure → recovery screen;
  `--demo` still works.
- Worker: sync command, progress events, cancel, failure, a second request while running, reload
  without losing selection.
- Reading: real body, attachment Save to a temp path, blocked schemes, missing blob, offline banner.
- Account switch/remove: data and credentials isolated (P6a already tests the backend; the UI must
  call it, not a second path).
- Screenshots: onboarding welcome/provider/form/error/progress, mailbox synced, offline, attachment
  save confirmation, settings. Light and dark; update attribution.
- Update `docs/ui/plan.md`, README, ROADMAP (M1 done only when this lands), `docs/development.md`.

## 8. Out of scope

SMTP, outbox, scheduled send, undo send, HTML rendering (M5), OAuth (P12), autoconfig/SRV, rules,
notifications, tray, daemon, multiple windows, contacts.

## Gates

`cargo xtask ci` green, clippy `-D warnings`, no `unwrap`/`expect`/`panic!` outside tests, no
password or message content in logs, no dead action, keyboard-only flows, 980×600 and dark verified.
Temp files only in `%TEMP%\opencode\p6b-postino`; do not commit.

## Report

Screens and states, worker design, tests, screenshot evidence, docs, and any deviation with the
reason.
