# Brief — P12a Gmail OAuth (XOAUTH2)

> **Package:** P12a. **Depends on:** P6/P7 accounts and net. **Parallel:** P12e packaging, P12f fuzz.
> **Do not touch:** `apps/postino-app/src/app.rs` or reading pane (P11a2). **Review:** blind
> security/credential review, one fix round.

## Outcome

A Gmail account can be added with OAuth 2.0 instead of an app password. IMAP and SMTP use XOAUTH2;
access tokens are memory-only, refresh tokens live in the OS keyring, refresh is bounded, and account
removal revokes the grant. Password/app-password accounts keep working unchanged.

## Decisions

- **BYO Google client ID + secret.** The desktop token endpoint still expects a client secret; it is
  supplied by the person, stored only in the keyring, never in the repo, argv, logs, screenshots or
  the store. No shared client secret in the binary.
- **Loopback + PKCE S256 + state**, system browser, `127.0.0.1:0`; no device flow. Scope
  `https://mail.google.com/`.
- Provider-agnostic `OAuthProvider` seam; Gmail is the first entry. App-password fallback remains
  visible when OAuth is unavailable; never downgrade silently.
- **No new dependency without a written review.** If the token HTTPS POST needs one, stop and report
  exact crate/version/licence/size; prefer the existing rustls/tokio stack and a bounded HTTP/1.1
  POST. No `reqwest`/browser runtime.
- No schema bump if the auth kind can be inferred from the keyring; if a migration is unavoidable,
  stop and report before writing one.

## Required

- `postino-net`: bounded token request/response, PKCE/state generation, loopback listener with
  timeout/cancel, token parse limits, XOAUTH2 SASL for IMAP/SMTP, typed errors, refresh 60 s before
  expiry, no retry loop on auth failure, no token in Debug/logs.
- `postino-platform`: system-browser opener seam (`ShellExecuteW`, no shell), keyring scopes
  `account:<id>:oauth` for refresh token and client secret, delete on account removal, no secret on
  disk.
- `postino-core`/CLI/app onboarding: `auth: oauth` path, BYO client id/secret from a no-echo prompt or
  stdin, account test/sync through XOAUTH2, account remove revokes, readable failure sentences,
  app-password path unchanged. Avoid `app.rs`; use `onboarding.rs` and existing seams.
- Honest limits: Google Testing-mode refresh tokens expire after 7 days; verification/CASA cost;
  Gmail only for this package.

## Tests

Pure PKCE/URL/state; scripted TLS token endpoint; fake IMAP/SMTP XOAUTH2; keyring mock; refresh,
expiry, revocation, denied consent, offline, oversized/malformed token response; no secret/token in
logs or errors; app-password regression. Package tests only; no full `cargo xtask ci`.

## Report (max 15 lines)

What changed, files, package tests/result, dependency decision, limits, deviations.
