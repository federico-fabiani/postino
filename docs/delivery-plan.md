# Delivery plan

> **Status:** active · **Updated:** 2026-10-24 · **Owner:** coordinator
>
> This document is the operating plan for the push from the M0/U0 shell to a finished v0.1. It is
> not a second roadmap: milestones and scope stay in [ROADMAP.md](../ROADMAP.md).

## 1. North star

A local-first, agent-first mail client that feels made by a small studio with taste:

- **Simple, complete, refined.** Every feature that exists is reachable from the GUI, the CLI and
  MCP, and none of the three is a second-class citizen.
- **Flawless GUI.** Forest/ivory/gold, Inter + JetBrains Mono, keyboard-first, no dead actions,
  no clipped footer, no layout that only works at one window size. Modern, not retro.
- **Rust engineering.** Small plain-data types, operations over ad-hoc commands, no `unwrap`, no
  hidden state, tests at four zooms. The bar is the Artcraft suite: quiet code, obvious seams.
- **Honest.** Demo-only paths say so; nothing claims SMTP, IDLE or server filters before they exist.
- **Delighters.** Small, non-obvious touches an advanced user notices: undo everywhere, palette
  commands, `?` sheet, thread-aware quoting, truthful sync state, saved searches, snooze.

## 2. Operating model

The coordinator (this session) owns scope, sequencing, review and commits. Development and
critique are delegated:

| Role | Context | Job |
|---|---|---|
| Coordinator | full repo history | write briefs, arbitrate, verify, commit |
| Developer agent | fresh + brief | implement one package, tests included |
| Blind UX reviewer | fresh, **no access to the developer's reasoning** | screenshots + headless UI, find UI/UX defects |
| Blind engineering reviewer | fresh | code, tests, security, operation parity |
| Fixer agent | findings only | apply accepted findings, no scope creep |

Rules:

- Every agent runs `opencode-go/deepseek-v4.1-flash#max` unless unavailable.
- A package is done when `cargo xtask ci` is green **and** a blind review reports no blocker.
- Reviews are severity-ranked (`blocker`, `major`, `minor`, `nit`) and must cite file/line or
  screenshot evidence. No praise, no restating the brief.
- Findings are accepted or rejected explicitly by the coordinator; rejected findings say why.
- One logical commit per package, green only. The user has delegated commit authority for this
  engagement; commits happen without asking.
- No package may add a feature the roadmap does not contain, and none may skip CLI/MCP parity.

## 3. Quality gates

1. **Contract:** every user-visible action is an operation with a schema, description and
   `read_only`/`destructive` flags; CLI and MCP derive from it.
2. **Tests:** unit/property, crate integration, end-to-end operation, headless UI render; a bug fix
   ships with a regression test that failed before.
3. **UI:** keyboard-only path, 980×600 minimum window, 125%/150% DPI, light and dark, empty/loading/
   error states, visible focus, no dead button.
4. **Screenshots:** `postino-app --screenshot <state> --out <png>` renders the real shell without
   input injection; docs images are regenerated from it, never hand-cropped.
5. **Docs:** operation table, README, `docs/mcp.md`, `docs/ui/plan.md`, `ROADMAP.md` updated in the
   same package.
6. **Honesty:** demo-only paths are labelled in the UI, CLI and MCP description.

## 4. Package queue

Status: `todo` · `active` · `review` · `done`.

| ID | Package | Outcome | Status |
|---|---|---|---|
| P0 | Screenshot harness | `--screenshot` states for shell, reading, compose, palette; dark variants; docs regenerated | done |
| P0.1 | Harness hardening | frozen clock, strict flags, no-hang watchdog, atomic PNG, parser/PNG tests | done |
| P1 | U0.5 labels | label create/rename/delete/apply/filter in engine, GUI, CLI, MCP; counts; keyboard | done |
| P2 | U0.5 polish | loading/error/empty states, reduced motion, a11y pass, final light/dark screenshots | todo |
| P3 | M1 store | SQLite (encryption seam + keyring), migrations, WAL, repository behind the engine | todo |
| P4 | M1 MIME | parser/serializer, sanitized HTML subset, plain-text alternative, attachment storage | todo |
| P5 | M1 IMAP | CAPABILITY/LOGIN/LIST/SELECT/FETCH/IDLE, fake-server tests, offline cache | todo |
| P6 | M1 onboarding | autodiscovery, Aruba app-password flow, multi-account model, real folder tree | todo |
| P7 | M2 SMTP | submission, outbox, idempotency, Sent append, crash safety | todo |
| P8 | M2 compose | attachments bytes, signatures per identity, scheduled send, undo send | todo |
| P9 | M3 organize | threads, FTS5 search, rules, label mapping, attachment preview/save | todo |
| P10 | M4 daemon | `postino-daemon`, named pipe/UDS + token, tray, autostart, clients | todo |
| P11 | M5 render | `BodyRenderer`, sandboxed webview, block/HTML composer, templates | todo |
| P12 | M6 ship | Gmail OAuth, Sieve, PGP, invites, packaging, security/fuzz | todo |

P0-P2 close U0. P3-P6 make M1 real; P7-P8 close M2. Each package may split when the brief is too
large; splits are recorded here.

## 5. Delighters backlog

Candidate small features, each cheap but noticeable. Ship only after the milestone they belong to:

- Saved searches and a "smart folder" chip row.
- `Ctrl+Z` undo for every mutation, with a single visible stack.
- Thread-aware quote header and "reply to list" where headers allow it.
- Snooze presets and a "returns at" badge.
- Recipient autocomplete from local history, with keyboard-only selection.
- Attachment size total with a warning before send.
- Per-sender remote-content allowlist with a visible trust badge.
- A true empty state for every filter, never a blank pane.
- Command palette that also runs operations by name (`messages_list`), for agents at a desk.

## 6. Known risks

- **No real mailbox in this environment.** IMAP/SMTP are developed against scripted fake servers
  and golden MIME files; a live Aruba/Gmail smoke test stays a manual release step.
- **Windows GNU toolchain.** Tests need `C:\msys64\mingw64\bin` on `PATH`; MSVC lacks `link.exe`.
- **Screenshot rendering** depends on a GPU surface; if eframe screenshots fail, fall back to
  `egui_kittest` + wgpu before resorting to OS capture.
- **Scope drift.** The demo engine must not grow features that M1 will throw away; new state goes
  behind the repository seam.
