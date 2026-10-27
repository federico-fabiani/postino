# Delivery plan

> **Status:** active · **Updated:** 2026-10-27 · **Owner:** coordinator
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
| P2a | Layout and modals | composer fits 980x600, modal palette, pinned sidebar editor, list proportions | done |
| P2b | States and a11y | empty/error/loading states, contrast floors, accessible rows, persistence, copy | done |
| P3d | Persistence correctness | atomic draft save, patch updates, account routing, revision and label sequences, keyring lock | done |
| P3a | Store spike | SQLCipher go/no-go with measured build time, size and encrypted-file proof | done |
| P3b | Store schema + keyring | platform keyring seam, schema v2, repository API, no-plaintext tests | done |
| P3c | Engine wiring | engine reads/writes the encrypted store; demo path unchanged; reopen test | done |
| P4a | MIME crate | parse/serialize, sanitized HTML allowlist, text alternative, bounds and fixtures | done |
| P4b | Attachment blobs | content-addressed encrypted blobs, engine/store plumbing, reopen test | done |
| P5a | IMAP client | CAPABILITY/LOGIN/LIST/SELECT/FETCH/IDLE, TLS, bounds, scripted fake server | done |
| P5b | Sync and cache | reconciliation, offline cache, sync status, 10k/kill-resume tests | done |
| P6a | Accounts backend | credentials, schema v4, account/sync operations, CLI and MCP, sensitive filtering | done |
| P6b | M1 onboarding UI | autodiscovery screen, Aruba app-password flow, app on real data | done |
| P6c | M1 reading + settings | text reading pane, safe links, attachment Save, full settings, chrome cleanup | done |
| P6c-fix | M1 review closure | filename-warning bypass, effective link destination, narrow-overlay Retry, demo Save, test depth, settings minors | done |
| P6d | M1 remote-content count | schema v6 `messages.remote_images`, carry `remote_images_blocked` MIME→sync→store, render stored truth | done |
| P7 | M2 SMTP | submission, outbox, idempotency, Sent append, crash safety | done |
| P8 | M2 compose | attachment bytes, signatures per identity, reply/forward recipients and headers, size caps | done |
| P9a | M3 search | read path without bodies, FTS5 query language, search ops/UI, v7 inert tables | done |
| P9b | M3 threads | thread derivation/merge/alias, conversation view, saved searches UI | done |
| P9c | M3 rules | local rules, preview, idempotent runs, undo, forward through P7 | done |
| P10 | M4 daemon | `postino-daemon`, named pipe/UDS + token, tray, autostart, clients | todo |
| P11 | M5 render | `BodyRenderer`, sandboxed webview, block/HTML composer, templates | todo |
| P12 | M6 ship | Gmail OAuth, Sieve, PGP, invites, packaging, security/fuzz | todo |

P0-P2 close U0. P3-P6d make M1 real; P7-P8 close M2. Each package may split when the brief is too
large; splits are recorded here. The P6c blind review found one filename-warning blocker and several
major UX/test gaps; P6c-fix closed them and the re-review found no blocker. P7 went through three
blind reviews and closure rounds: duplicate submission after a cancel, raw server text, revival
ordering, a damaged append envelope, live-lease expiry, append ownership, Sent dedupe, external
refresh, draft-id collisions and append backoff were all fixed before the squash merge. P6d is done
(schema v6 carries the parsed remote-image count through sync and the reading pane). P8 is merged
after its blind review closure: real attachment bytes, per-account signatures, reply/reply-all/forward
headers and the 20/25 MiB caps, with no schema change. M2 is complete. P9a is done after its review
closure: schema v7, the body-free read path, FTS5 query language, operations, UI and 100k evidence.
The search first-page target is met (51 ms P50 at 100k); open misses its 150 ms target and RSS is
unverified, both stated in the architecture table. P9b is done in two green commits: P9b1 is the
reference-chain engine, thread cache, operations, CLI/MCP and 20k/100k evidence; P9b2 is the
conversation view, saved searches, screenshots and docs. A blind review found a missing
`messages(account_id,message_id)` index (1.29 s per reference at 100k) and a sent-draft ghost thread;
the closure adds the derived indexes, refresh of the old draft thread, store-backed flag counts, live
thread pages, subject-fallback stability, saved-search freshness/account cleanup, conversation Enter,
thread paging and partial-batch undo. The account-wide `threads_list` target is met (80.7 ms P50 /
82.1 ms P95 at 100k) and 100 real 3-reference inserts take 307 ms (P95 6.0 ms); the folder-scoped list
misses it (356–359 ms P95) and is reported in the architecture/threat-model tables. P9b creates derived
indexes idempotently on normal open but has no schema version change. P9c is the last M3 package and is
done: local rules, preview, idempotent runs, state-checked undo and forwarding through P7 on the
inherited v7 rule tables, with no migration and no new dependency. A rule is a validated search query
plus 1-5 actions; runs are per rule in priority order with stop, received mail only, an
`ingested_at >= created_at` horizon and a 2 000-candidate cap. The journal keeps ids/folders/labels and
booleans only and 20 runs; a forward is one P7 outbox row with `Auto-Submitted`/`X-Postino-Rule`, a
loop guard in encrypted meta and a 50/run cap. MCP gates `rules_run` and forward payloads behind
`--allow-send`; the Rules page has list/editor/preview/run/undo at 980×600 in both themes.

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
