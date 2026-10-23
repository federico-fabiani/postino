# Postino product

> **Status:** M0 · **Last updated:** 2026-10-23

## 1. Vision

A mail client that opens instantly, keeps your mail on your machine, and lets both you and your
agents do everything from one honest surface. Not a suite. Not a web app in a trench coat. A
postman: it carries the mail and gets out of the way.

The name is **Postino**. The product is Postino, the command is `postino`, the desktop binary is
`postino-app`, and the MCP server is `postino mcp` (a `postino-mcp` shim can follow for clients
that insist on a dedicated executable).

## 2. Principles

1. **Local-first.** Mail is cached, indexed and searched locally. The server remains the source of
   truth; the local copy is yours and works offline.
2. **Agent-first.** Every feature is an operation. If the GUI can do it, the CLI and MCP can do it,
   with the same schema and the same validation.
3. **Fast by construction.** No embedded browser in the shell, no synchronous network on the UI
   thread, no unbounded lists. The first screen appears before sync starts.
4. **Boring where it counts.** IMAP and SMTP, not a private sync protocol. SQLite, not a bespoke
   store. Standard MIME, standard crypto, standard keyring.
5. **Honest.** If a provider cannot do it, the product says so. No fake read receipts, no promised
   avatars, no "AI magic" that quietly uploads mail.
6. **Warm, not cute.** Forest, ivory and gold; a native interface with a human voice and rigorous
   behaviour underneath.

## 3. Who it is for

- A professional who lives in one or two mailboxes (Aruba, Gmail, Fastmail, a company IMAP) and
  wants a fast, keyboard-first client without a subscription.
- A developer or operator who wants to script mail and give an agent controlled access to it.
- Someone who cares where the mail is stored and does not want a cloud account in the middle.

## 4. What "good" looks like

- Cold start under 300 ms on a normal laptop; the message list is usable before the first sync
  finishes.
- Searching 100,000 messages returns the first page in under 100 ms.
- Reading a message never blocks on the network.
- An agent can list, search, read, draft and send through MCP, with the same result as the GUI.
- Nothing leaves the machine except the user's own IMAP/SMTP traffic.
- A malformed message, attachment or MCP call produces a readable error, never a crash.

## 5. Scope

### v0.1 — usable every day

- IMAP/SMTP with autodiscovery, TLS, app passwords and multi-account data model.
- Local encrypted cache, offline reading, IDLE sync.
- Folder rail, unified inbox, threads, attachments.
- Read HTML as a sanitized subset; complex mail can open in the browser.
- Compose To/Cc/Bcc, signatures, attachments, reply/reply-all/forward.
- Draft autosave, outbox, append to Sent.
- Read/starred flags, archive/delete/move.
- Full-text search with `from:`, `to:`, `subject:`, `has:attachment`, `is:unread`, `before:`,
  `after:`.
- Notifications, tray, keyboard shortcuts, command palette.
- CLI and MCP for everything above.

### v1 — professional

- Multi-account UI, identities and aliases.
- Tags/labels, local rules, templates, snooze.
- Scheduled send, undo send, follow-up reminders.
- Contacts, autocomplete, groups, vCard import.
- Block editor + HTML/Tailwind advanced composer, preview, remote-content blocking, tracking-pixel
  stripping.
- Server-side filters and vacation responder where the provider exposes Sieve/ManageSieve.
- Gmail OAuth (bring-your-own client id or a verified app), Gmail labels.
- PGP via an external agent/library and S/MIME later.
- Calendar invites (`.ics`) display and response, one-click unsubscribe.
- Export `.eml`/`.mbox`, backup, Thunderbird import.
- Italian/English UI, dark mode, accessibility audit, signed packages.

### Explicit non-goals (or not client-side)

- **Recipient-visible avatars.** A recipient sees your logo only through BIMI (DNS + VMC), a
  provider profile, or your signature. Postino can show contact avatars locally and put a logo in
  the signature; it cannot make another client display one.
- **Auto-reply and auto-forward with the app closed.** These are server features. Postino will
  generate/upload Sieve scripts where the provider supports it and document the rest.
- **Reliable read receipts.** Most providers ignore or strip them. Postino will not pretend.
- **Push without IMAP IDLE.** Where the provider does not support IDLE, polling is the floor.
- **A Postino sync server.** IMAP already is the sync.
- **Absolute security.** We publish a threat model and defend it; "unhackable" is not a claim.

## 6. Feature details people notice when missing

- Unread count per folder, a global unread view, and "mark all read" with undo.
- Reply quoting that preserves the thread and never mangles the recipient list.
- Bcc that stays bcc in the envelope and never appears in headers.
- Draft recovery after a crash, and drafts that do not duplicate across devices.
- Attachments: preview, save, drag and drop, size warnings, executable warnings.
- Search that finds body text, not just subjects.
- Rules that can move, tag, mark read, delete, forward, or run a webhook later.
- Signature per identity, including plain-text fallback.
- Scheduled send with a visible outbox and cancellation.
- Undo send for a few seconds, visibly counting down.
- Remote content blocked with a per-sender "always load" choice.
- Keyboard: `J`/`K`, `R`, `A`, `F`, `Ctrl+K`, `Esc`.
- A tray icon that tells the truth about sync state.

## 7. v0.1 acceptance

A person can add an Aruba account, sync the inbox, read and search mail, reply with a signature and
an attachment, save a draft, send it, and see it on a second device, using only Postino. Every one
of those actions also works through `postino` and `postino mcp`.

M0 does not meet this yet. It establishes the shell, the operation contract and the test harness.
