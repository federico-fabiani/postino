# Brief — P7 SMTP submission and outbox

> **Package:** P7 · **Depends on:** P6b/P6c (app on real mail), P4a (serializer), P4b (blobs) ·
> **Next:** P8 compose attachments/signatures, P9 organize
> **Owner:** developer agent · **Review:** blind security + crash/duplicate evidence
>
> The first package where Postino can send a real message. Until now every “send” was a local move;
> after P7 it is a submission with a journal, and the UI must stop saying “Invia (locale)”.

## Outcome

- Compose can queue and send a message through the account's SMTP server, with the exact RFC 5322
  bytes persisted before any network call.
- An outbox survives restarts: scheduled send, retry with backoff, cancel before send, a clear
  uncertain state after a crash during DATA, and a Sent append that is retried independently.
- Credentials stay in the keyring; the SMTP password may be the IMAP one (Aruba, Gmail app password)
  or a separate secret.
- CLI, MCP and UI all use the same operations. MCP sending is off unless the server is started with
  `--allow-send`.

## 1. SMTP client in `postino-net`

- `smtp` module: connect implicit TLS (465) or STARTTLS (587), EHLO, parse capabilities, AUTH PLAIN
  and LOGIN, MAIL FROM, RCPT TO, DATA, RSET, NOOP, QUIT.
- Extensions: `SIZE` (refuse locally before DATA if over), `8BITMIME`, `SMTPUTF8` (only when the
  envelope/headers need it and the server advertises it), `DSN` (optional flags), `PIPELINING` only
  if implemented and tested; otherwise sequential.
- Bounds: line 8 KiB, response 64 KiB, recipient cap (100 per message), command/response timeouts,
  DATA termination correct (`\r\n.\r\n`), dot-stuffing, no bare LF.
- Errors typed: `Connect`, `Tls`, `Auth`, `Rejected { code, stage, message }`, `Transient`,
  `Permanent`, `Timeout`, `Closed`, `TooLarge`, `Unsupported`. 4xx is transient; 5xx permanent.
- Same TLS policy as IMAP: OS verifier, extra roots for tests, no skip option. Password zeroised and
  redacted. Never log the message body or the recipient list beyond a count.

## 2. Schema v5 and credentials

Migration v5, additive:

```sql
ALTER TABLE account_servers ADD COLUMN smtp_host TEXT;
ALTER TABLE account_servers ADD COLUMN smtp_port INTEGER;
ALTER TABLE account_servers ADD COLUMN smtp_tls TEXT;
ALTER TABLE account_servers ADD COLUMN smtp_username TEXT;
CREATE TABLE outbox(
    id TEXT PRIMARY KEY,
    account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    idempotency_key TEXT NOT NULL UNIQUE,
    created_at INTEGER NOT NULL,
    send_at INTEGER NOT NULL,
    state TEXT NOT NULL,              -- queued | sending | sent | append_pending | uncertain | failed | cancelled
    attempts INTEGER NOT NULL DEFAULT 0,
    last_error TEXT,
    raw_blob_hash TEXT NOT NULL,
    envelope_json TEXT NOT NULL
);
CREATE INDEX outbox_due ON outbox(state, send_at);
```

- Provider presets: Aruba `smtps.aruba.it:465 implicit`; Gmail `smtp.gmail.com:465 implicit` (app
  password, 2FA). Custom SMTP fields in onboarding (P6b form extended).
- Credential scopes: `account:<id>:smtp`; if absent, fall back to the IMAP credential and record that
  choice in the docs, never in the database as a password.
- `account_add`/`account_update` accept SMTP fields; `--smtp-*` CLI flags, password from the same
  prompt or a second one only when it differs.

## 3. Send pipeline

1. `compose` produces an `OutgoingMessage`; `postino-mime::serialize` returns bytes + envelope.
2. The bytes go to the blob store (content-addressed, encrypted); the outbox row is inserted in one
   store transaction with the hash, envelope JSON, account id and `send_at`.
3. The worker leases a due row (`state = sending`, `attempts += 1`) in a transaction, then submits.
4. SMTP accepted → state `append_pending`; IMAP APPEND to the account's Sent with the raw bytes and
   `\Seen`; success → `sent`. APPEND failure keeps `append_pending` and retries with backoff,
   independently of SMTP (the message is already sent; never resend it).
5. A crash during DATA leaves `sending`; on restart, rows older than a lease window become
   `uncertain`, never auto-retried. The UI says “Invio incerto: controlla in Inviati” and offers
   “Segna come inviato” / “Riprova manualmente”.
6. `send_at` gives scheduled send; a 5–10 s delay before the first attempt gives undo send.
   `outbox_cancel` works until `sending`; after that only the uncertain policy applies.
7. Retry: transient errors with exponential backoff (30 s → 1 h, max 8 attempts), permanent errors
   stop and surface the server text (no credentials, no body). Auth failure stops the queue for that
   account with an actionable message.

## 4. Operations, CLI, MCP

| Operation | Notes |
|---|---|
| `message_send` | Send a draft now (queue it, with the undo window); write. Sensitive in the sense that it leaves the machine? No: it is the product's purpose, but MCP must still respect `--allow-send`. |
| `send_schedule` | Queue a draft at `send_at`. |
| `outbox_list` | Read-only rows with state, attempts, last error, recipient count, subject. |
| `outbox_cancel` | Cancel a queued/scheduled row; destructive? It discards the send, not mail; mark destructive false but describe it. |
| `outbox_retry` | Manual retry of `failed`/`uncertain` with explicit confirmation. |
| `send_dry_run` | Return the exact RFC 822 bytes (base64) and envelope without queueing; read-only. |

- MCP: all outbox reads are available; `message_send`/`send_schedule` are refused unless the server
  was started with `--allow-send` (default off) and are listed as disabled with the reason otherwise.
- CLI: `postino send --draft id [--at …] [--dry-run]`, `postino outbox list|cancel|retry`, password
  never in argv.
- The UI toast gets Undo for the delay window; after that “Annulla” is gone and the outbox shows the
  item.

## 5. Tests

Fake SMTP server in `postino-testkit`: greeting, EHLO, capabilities, AUTH PLAIN/LOGIN failure and
success, 4xx/5xx per stage, SIZE refusal, SMTPUTF8 required/absent, dot-stuffing, DATA timeout, drop
after DATA accepted (uncertain), slow response, STARTTLS and implicit TLS with the test CA, wrong
hostname refused, password never in the transcript.

- Serialize → queue → send with the fake SMTP, assert the exact bytes and envelope, Bcc only in
  RCPT, Sent APPEND with `\Seen`.
- Restart after queue, after `sending`, after `sent` before APPEND, after APPEND; assert no duplicate
  SMTP submission and the final state.
- Transient 451 retries with backoff; permanent 550 stops with the server text; auth failure stops.
- Scheduled `send_at` is honoured; cancel before due; cancel after `sending` is refused.
- CLI and MCP: `--allow-send` off refuses; dry run returns bytes without a queue row; no password in
  output.
- A 25 MiB attachment and a 10-recipient message; recipient cap and SIZE refusal.

## 6. Docs

- README: real send, provider notes (Aruba/Gmail app passwords), outbox semantics and the uncertainty
  policy; no claim of read receipts or server-side scheduling.
- Architecture: SMTP pipeline, outbox states, leases, crash windows, APPEND decoupling, credentials.
- Threat model: MCP `--allow-send`, recipient caps, no body logging, uncertain state, SMTP TLS.
- `docs/mcp.md`: the send toggle and the new tools.
- ROADMAP: M2 send/drafts/outbox; leave threading/search/rules for M3.

## 7. Out of scope

HTML compose/rendering, OAuth, Sieve, PGP, read receipts, server-side scheduling, mailing lists,
tracking pixels, BCC splitting into multiple submissions (M8 if needed), calendar invites.

## Gates

`cargo xtask ci` green, clippy `-D warnings`, no secrets/bodies/addresses in logs, crash-window
tests, no duplicate submission after any restart point. Temp files only in
`%TEMP%\opencode\p7-postino`; do not commit.

## Report

SMTP capabilities covered, outbox state machine, crash/duplicate evidence, test map, CLI transcripts,
and any deviation with the reason.
